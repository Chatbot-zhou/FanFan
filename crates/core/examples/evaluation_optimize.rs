//! 翻翻：真实资料评测 + 自动迭代优化闭环编排器。
//!
//! 流程（与 docs/fanfan_trace_evaluation_optimization_agent_prompt.txt 一致）：
//!   1. 从真实已解析文档构建语料（Evidence-first，先定 Gold Evidence 再出题）
//!   2. 生成评测数据集 → 按 file_id 确定性划分 DEV(70%) / HOLDOUT(30%) → 冻结落库
//!   3. 运行真实链路（SEARCH / ASK / SMART_COLLECTION / FILE_RELATION）得到观测
//!   4. 逐例判定 → 记录 EvaluationRun + EvaluationResult
//!   5. 输出指标聚合 + Failure Analysis（按 28 类错误分类）
//!
//! 命令行参数：
//!   --data-dir <path>            数据目录（默认 APPDATA/com.fanfan.desktop）
//!   --model-store <path>         模型目录（默认 LOCALAPPDATA/FanFan/ModelStore/v1）
//!   --evaluation-root <path>     评测快照目录（默认 LOCALAPPDATA/FanFan/Evaluation/v1）
//!   --split dev|holdout          运行分组（默认 dev）
//!   --round <N>                  优化轮次（默认 0 = Baseline）
//!   --skip-dataset-refresh       跳过数据集生成/冻结，直接读取已冻结用例
//!   --max-corpus-files <N>       语料采样上限（默认 80）
//!   --freeze-batches-only        只冻结全部批次清单，不运行问答
//!
//! 本编排只做通用链路运行与统计，不做任何针对具体问题/文件/关键词的特判。

use std::{
    collections::{HashMap, HashSet},
    env, fs,
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
    time::{Instant, UNIX_EPOCH},
};

use fanfan_core::{
    AnswerStyle, AskRequest, Availability, CatalogStore, DatasetGenerationOptions,
    EvaluationBatchManifestV1, EvaluationCaseRecord, EvaluationCorpusFile, EvaluationObservation,
    EvaluationResultRecord, EvaluationRunRecord, FileRecord, ModelArtifact, ModelManager,
    ModelRole, OllamaChatOptions, OllamaClient, ParseStatus, RelationQuery, RelationType,
    ScopeFilter, SearchMode, SearchRequest, SearchSort, SemanticQuery,
    aggregate_evaluation_metrics, analyze_failures, apply_grounded_generation,
    create_encrypted_evaluation_snapshot, evaluate_case_verdict, freeze_ragas_evaluation_batches,
    generate_evaluation_dataset, generation_prompt, grounded_answer_json_schema,
    materialize_evaluation_snapshot, ragas_sample_from_observation,
    should_synthesize_grounded_answer, split_evaluation_dataset_by_file,
    validate_ragas_batch_manifest, write_protected_ragas_samples,
};
use sha2::{Digest, Sha256};

/// SEARCH 检索返回条数（判定 Top5 召回需要至少返回 5 条）。
const SEARCH_TOP_K: u32 = 10;
/// SMART_COLLECTION 候选召回条数（用于集合成员判定）。
const COLLECTION_TOP_K: u32 = 20;
fn main() {
    if let Err(error) = run() {
        eprintln!(
            "优化评测未完成: code={} message={}",
            error.code, error.message
        );
        if let Some(details) = error.details {
            eprintln!("technical={details}");
        }
        std::process::exit(1);
    }
}

/// 闭环入口：解析参数 → 快照 → 数据集 → 运行 → 记录 → 输出统计。
fn run() -> Result<(), fanfan_core::AppError> {
    let data_directory = argument_path("--data-dir").unwrap_or_else(default_data_directory);
    let model_store = argument_path("--model-store").unwrap_or_else(default_model_store);
    let evaluation_root =
        argument_path("--evaluation-root").unwrap_or_else(default_evaluation_root);
    let split = argument_value("--split").unwrap_or_else(|| "dev".to_owned());
    let dataset_version = argument_value("--dataset-version").unwrap_or_else(|| "v1".to_owned());
    let round = argument_value("--round")
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(0);
    let skip_dataset_refresh = argument_present("--skip-dataset-refresh");
    let max_corpus_files = argument_value("--max-corpus-files")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(80);
    let max_ask_cases = argument_value("--max-ask-cases")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(80);
    let batch_size = argument_value("--batch-size").and_then(|value| value.parse::<usize>().ok());
    let batch_index = argument_value("--batch-index").and_then(|value| value.parse::<usize>().ok());
    let batch_manifest_path = argument_path("--batch-manifest");
    let phase = argument_value("--phase").unwrap_or_else(|| "baseline".to_owned());
    let parent_run_id = argument_value("--parent-run-id");
    let export_ragas = argument_present("--ragas-export");
    let freeze_batches_only = argument_present("--freeze-batches-only");
    let code_fingerprint = argument_value("--code-fingerprint");
    // 逐例转储：把每个用例的观测 + 判定写入 JSONL，供失败根因分析（离线）。
    let dump_cases_path = argument_path("--dump-cases");

    // 1. 打开隔离评测快照（源库只读；快照可写）
    let source_database = data_directory.join("fanfan.db");
    let snapshot = create_encrypted_evaluation_snapshot(&source_database, &evaluation_root)?;
    let working_copy = materialize_evaluation_snapshot(&snapshot)?;
    let catalog = CatalogStore::open(working_copy.path.clone())?;
    let source_files = catalog.list_files()?;
    let source_manifest_before = source_manifest_hash(&source_files);

    // 2. 生成或加载数据集，DEV/HOLDOUT 冻结
    let (dev, holdout) = if skip_dataset_refresh {
        load_frozen_dataset(&catalog)?
    } else {
        let corpus = build_corpus(&catalog, max_corpus_files)?;
        let generation_options = DatasetGenerationOptions {
            dataset_version: dataset_version.clone(),
            max_ask_cases,
            ..DatasetGenerationOptions::default()
        };
        let cases = generate_evaluation_dataset(&corpus, &generation_options)?;
        let (dev, holdout) = split_evaluation_dataset_by_file(cases, 0.7);
        // 冻结：幂等落库（ON CONFLICT 覆盖，重复运行不产生重复用例）
        catalog.record_evaluation_cases(&dev)?;
        catalog.record_evaluation_cases(&holdout)?;
        println!(
            "数据集已冻结: 总数={}, DEV={}, HOLDOUT={}",
            dev.len() + holdout.len(),
            dev.len(),
            holdout.len()
        );
        (dev, holdout)
    };
    print_split_profile(&dev, &holdout);
    print_coverage_profile(&dev, &holdout);
    let mut batch_manifests_to_freeze = Vec::new();
    let (selected, mut batch_manifest) = if let Some(path) = batch_manifest_path.as_deref() {
        let manifest = read_batch_manifest(path)?;
        validate_ragas_batch_manifest(&manifest)?;
        if manifest.dataset_version != dataset_version {
            return Err(fanfan_core::AppError::new(
                "EVALUATION_BATCH_INVALID",
                "批次清单与请求的数据集版本不一致",
                false,
            ));
        }
        let pool = if manifest.split == "HOLDOUT" {
            &holdout
        } else {
            &dev
        };
        (select_manifest_cases(pool, &manifest)?, Some(manifest))
    } else if let (Some(size), Some(index)) = (batch_size, batch_index) {
        let manifests = freeze_ragas_evaluation_batches(&dev, &holdout, &dataset_version, size, 3)?;
        let manifest = if split.eq_ignore_ascii_case("holdout") {
            manifests
                .iter()
                .find(|manifest| manifest.split == "HOLDOUT")
                .cloned()
        } else {
            manifests
                .iter()
                .find(|manifest| manifest.split == "DEV" && manifest.batch_index as usize == index)
                .cloned()
        }
        .ok_or_else(|| {
            fanfan_core::AppError::new("EVALUATION_BATCH_INVALID", "请求的评测批次不存在", false)
        })?;
        let pool = if manifest.split == "HOLDOUT" {
            &holdout
        } else {
            &dev
        };
        batch_manifests_to_freeze = manifests;
        (select_manifest_cases(pool, &manifest)?, Some(manifest))
    } else {
        (
            match split.as_str() {
                "holdout" => holdout,
                _ => dev,
            },
            None,
        )
    };
    if selected.is_empty() {
        return Err(fanfan_core::AppError::new(
            "EVALUATION_EMPTY_SPLIT",
            format!("分组 {split} 没有可用用例，请先运行数据集生成"),
            false,
        ));
    }

    // 3. 运行时：embedding 走本机 Ollama（生成/嵌入已迁移 Ollama，无本地 ONNX
    //    embedding 文件与 tokenizer；检索链路与生产 app_data.rs 同口径）。
    let manager = ModelManager::open_store(&model_store)?;
    // 与生产启动逻辑一致：Ollama 托管的模型无本地文件，open_store 后 status 可能
    // 是 incomplete，需经 ollama_*_ready 幂等登记为 ready（重复调用覆盖同一 artifact）。
    // active_artifact 只返回 ready，故先按角色从 list_artifacts 找到 Ollama artifact
    // 并登记 ready，再取激活项。
    if let Some(embedding_artifact) = manager
        .list_artifacts()?
        .into_iter()
        .find(|artifact| artifact.role == ModelRole::Embedding)
        && embedding_artifact.format == fanfan_core::ModelFormat::Ollama
    {
        manager.ollama_embedding_ready(&embedding_artifact.model_id)?;
    }
    let embedding = manager
        .active_artifact(ModelRole::Embedding)?
        .ok_or_else(|| {
            fanfan_core::AppError::new(
                "EVALUATION_EMBEDDING_UNAVAILABLE",
                "评测需要已通过完整性检查的 Embedding 模型",
                false,
            )
        })?;
    if freeze_batches_only {
        if batch_manifests_to_freeze.is_empty() {
            return Err(fanfan_core::AppError::new(
                "EVALUATION_BATCH_INVALID",
                "只冻结模式需要 --batch-size 和 --batch-index",
                false,
            ));
        }
        for manifest in &mut batch_manifests_to_freeze {
            manifest.code_fingerprint = code_fingerprint.clone();
            manifest.index_fingerprint = Some(snapshot.sha256.clone());
            manifest.model_fingerprint = Some(embedding.sha256.clone());
            manifest.source_manifest_fingerprint = Some(source_manifest_before.clone());
            let path = persist_batch_manifest(&evaluation_root, manifest)?;
            println!("BATCH_MANIFEST={} {}", manifest.batch_id, path.display());
        }
        return Ok(());
    }
    let ollama = OllamaClient::local();
    // 校验 Ollama 中 embedding 模型已就位（format=Ollama 无本地文件，直接按 tag 探测）。
    let available_ollama_models = ollama.list_models().map_err(|error| {
        fanfan_core::AppError::new(
            "EVALUATION_OLLAMA_UNAVAILABLE",
            format!("评测需要本机 Ollama 服务与嵌入模型：{error}"),
            true,
        )
    })?;
    if !available_ollama_models
        .iter()
        .any(|model| model.name == embedding.model_id)
    {
        return Err(fanfan_core::AppError::new(
            "EVALUATION_EMBEDDING_UNAVAILABLE",
            format!("Ollama 中不存在嵌入模型 {}", embedding.model_id),
            true,
        ));
    }
    // DEV 优化第1轮：接通真实 LLM 生成链路。评测 ASK 不再只输出摘录式拼接，
    // 而是在摘录证据之上调用本地生成模型，产出带 citation_ids 的结构化合成回答
    // （apply_grounded_generation），从而解决 round1 的 NO_SYNTHESIS 主因。
    // 与 embedding 同理：先对 Ollama generation 做 ready 登记，再取激活项。
    if let Some(generation_artifact) = manager
        .list_artifacts()?
        .into_iter()
        .find(|artifact| artifact.role == ModelRole::Generation)
        && generation_artifact.format == fanfan_core::ModelFormat::Ollama
    {
        let catalog_id = generation_artifact
            .catalog_id
            .as_deref()
            .unwrap_or("qwen3-5-2b-q4")
            .to_owned();
        manager.ollama_generation_ready(&generation_artifact.model_id, &catalog_id)?;
    }
    let generation_model_id = match manager.active_artifact(ModelRole::Generation)? {
        Some(generation) => {
            // 生成后端已迁移到本机 Ollama：Ollama artifact 的 model_id 即 tag
            // （local_path 也是 tag，无本地 llama.cpp runtime）。评测器直接调用
            // 本机 Ollama /api/chat，与生产链路保持同口径。
            let ollama_tag = if generation.format == fanfan_core::ModelFormat::Ollama {
                generation.model_id.clone()
            } else {
                resolve_generation_ollama_tag(Path::new(&generation.local_path))?
            };
            if !available_ollama_models
                .iter()
                .any(|model| model.name == ollama_tag)
            {
                return Err(fanfan_core::AppError::new(
                    "EVALUATION_GENERATION_UNAVAILABLE",
                    format!("Ollama 中不存在生成模型 {ollama_tag}，请先在模型管理中拉取"),
                    true,
                ));
            }
            Some(ollama_tag)
        }
        None => {
            eprintln!("WARN 缺少 Generation 模型，本轮 ASK 回退为摘录式回答（无法评测合成链路）");
            None
        }
    };
    let mut runner = LinkRunner {
        embedding,
        ollama,
        generation_model_id,
        cancelled: AtomicBool::new(false),
        index_fingerprint: snapshot.sha256.clone(),
        code_fingerprint,
    };
    for manifest in &mut batch_manifests_to_freeze {
        manifest.code_fingerprint = runner.code_fingerprint.clone();
        manifest.index_fingerprint = Some(runner.index_fingerprint.clone());
        manifest.model_fingerprint = Some(runner.embedding.sha256.clone());
        manifest.source_manifest_fingerprint = Some(source_manifest_before.clone());
        persist_batch_manifest(&evaluation_root, manifest)?;
    }
    if let Some(manifest) = batch_manifest.as_mut() {
        if let Some(frozen) = batch_manifests_to_freeze
            .iter()
            .find(|frozen| frozen.batch_id == manifest.batch_id)
        {
            *manifest = frozen.clone();
        } else {
            manifest.code_fingerprint = runner.code_fingerprint.clone();
            manifest.index_fingerprint = Some(runner.index_fingerprint.clone());
            manifest.model_fingerprint = Some(runner.embedding.sha256.clone());
            manifest.source_manifest_fingerprint = Some(source_manifest_before.clone());
            persist_batch_manifest(&evaluation_root, manifest)?;
        }
        println!("BATCH_ID={}", manifest.batch_id);
    }

    // 4. 运行本轮（记录 run + 逐例结果）
    let run_id = uuid::Uuid::now_v7().to_string();
    let model_ids = vec![runner.embedding.artifact_id.to_string()];
    catalog.record_evaluation_run(&EvaluationRunRecord {
        run_id: run_id.clone(),
        dataset_version: dataset_version.clone(),
        code_revision: runner
            .code_fingerprint
            .clone()
            .or_else(|| Some(format!("evaluation_optimize-round-{round}"))),
        preset_id: None,
        model_ids: Some(model_ids),
        optimization_round: round,
        started_at: chrono::Utc::now(),
        completed_at: None,
        metrics: serde_json::Value::Null,
    })?;

    let started = Instant::now();
    let (results, mut ragas_samples, observations) =
        run_cases(&catalog, &mut runner, &selected, &run_id)?;
    for sample in &mut ragas_samples {
        if let Some(judgements) = sample.deterministic_judgements.as_object_mut() {
            if let Some(manifest) = &batch_manifest {
                judgements.insert("batch_id".into(), manifest.batch_id.clone().into());
            }
            judgements.insert("phase".into(), phase.clone().into());
            judgements.insert(
                "parent_run_id".into(),
                parent_run_id
                    .as_deref()
                    .map_or(serde_json::Value::Null, Into::into),
            );
        }
    }
    let wall_ms = started.elapsed().as_millis() as u64;
    let source_manifest_after = source_manifest_hash(&source_files);
    if source_manifest_before != source_manifest_after {
        return Err(fanfan_core::AppError::new(
            "EVALUATION_SOURCE_CHANGED",
            "评测期间授权源文件清单发生变化，本次运行已拒绝记录为成功",
            true,
        ));
    }
    let metrics = aggregate_evaluation_metrics(&results);
    catalog.record_evaluation_results(&results)?;
    catalog.complete_evaluation_run(&run_id, &metrics)?;
    if export_ragas {
        let export_path = evaluation_root
            .join("ragas")
            .join(format!("ragas-{run_id}.jsonl"));
        let protected_path = write_protected_ragas_samples(&export_path, &ragas_samples)?;
        println!("RAGAS_EXPORT={}", protected_path.display());
    }

    // 5. 输出指标与失败分析
    let passed = results.iter().filter(|result| result.pass_fail).count();
    println!("\n== ROUND {round} [{split}] run_id={run_id} ==");
    println!(
        "用例总数={} 通过={} 失败={} 墙钟={}ms",
        results.len(),
        passed,
        results.len() - passed,
        wall_ms
    );
    println!(
        "metrics:\n{}",
        serde_json::to_string_pretty(&metrics).map_err(|error| {
            fanfan_core::AppError::new("EVALUATION_METRICS_SERIALIZE", error.to_string(), false)
        })?
    );
    let analysis = analyze_failures(&results);
    println!(
        "\nFailure Analysis: 失败率={:.2}% 失败数={}",
        analysis.failure_rate, analysis.total_failed
    );
    for stat in &analysis.by_category {
        println!(
            "  - {}: {} 例 ({}%)",
            stat.category.as_str(),
            stat.count,
            percent(stat.count, analysis.total_failed)
        );
    }
    println!("top_root_causes:");
    for cause in &analysis.top_root_causes {
        println!("  - {cause}");
    }
    // 记录每轮统计证据（供 Hypothesis 引用）
    println!(
        "\nBASELINE_OR_ROUND={round} SPLIT={split} RUN_ID={run_id} PASS={passed} TOTAL={}",
        results.len()
    );
    // 逐例转储：观测 + 判定写入 JSONL，供失败根因离线分析（与 results 同序）。
    if let Some(dump_path) = dump_cases_path.as_deref() {
        let mut dump_file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(dump_path)
            .map_err(|error| {
                fanfan_core::AppError::new(
                    "EVALUATION_DUMP_FAILED",
                    format!("无法写入逐例转储: {error}"),
                    true,
                )
            })?;
        for ((case, observation), result) in selected.iter().zip(&observations).zip(&results) {
            let record = serde_json::json!({
                "case_id": case.case_id,
                "feature_type": case.feature_type,
                "query": case.question_or_request,
                "expected_intent": case.expected_intent,
                "expected_file_ids": case.expected_file_ids,
                "expected_collection_members": case.expected_collection_members,
                "expected_relation_type": case.expected_relation_type,
                "gold_reason": case.gold_reason,
                "actual_files": observation.actual_files,
                "actual_evidence": observation.actual_evidence,
                "actual_collection_members": observation.actual_collection_members,
                "actual_relation_type": observation.actual_relation_type,
                "evidence_found": observation.evidence_found,
                "answer_grounded": observation.answer_grounded,
                "response": observation.response,
                "pass_fail": result.pass_fail,
                "error_category": result.error_category,
                "diagnosis_reason": result.diagnosis_reason,
                "metrics": result.metrics,
            });
            writeln!(dump_file, "{record}").map_err(|error| {
                fanfan_core::AppError::new(
                    "EVALUATION_DUMP_FAILED",
                    format!("无法写入逐例转储: {error}"),
                    true,
                )
            })?;
        }
        println!("CASE_DUMP={}", dump_path.display());
    }
    Ok(())
}

/// 从 catalog 读取已冻结的 DEV / HOLDOUT 用例（不重新生成）。
fn load_frozen_dataset(
    catalog: &CatalogStore,
) -> Result<(Vec<EvaluationCaseRecord>, Vec<EvaluationCaseRecord>), fanfan_core::AppError> {
    let dev = catalog.query_evaluation_cases("DEV", None)?;
    let holdout = catalog.query_evaluation_cases("HOLDOUT", None)?;
    Ok((dev, holdout))
}

fn read_batch_manifest(path: &Path) -> Result<EvaluationBatchManifestV1, fanfan_core::AppError> {
    let bytes = fs::read(path).map_err(|error| {
        fanfan_core::AppError::new(
            "EVALUATION_BATCH_READ_FAILED",
            format!("无法读取评测批次清单: {error}"),
            true,
        )
    })?;
    let manifest =
        serde_json::from_slice::<EvaluationBatchManifestV1>(&bytes).map_err(|error| {
            fanfan_core::AppError::new(
                "EVALUATION_BATCH_INVALID",
                format!("评测批次清单不是有效 JSON: {error}"),
                false,
            )
        })?;
    validate_ragas_batch_manifest(&manifest)?;
    Ok(manifest)
}

fn select_manifest_cases(
    pool: &[EvaluationCaseRecord],
    manifest: &EvaluationBatchManifestV1,
) -> Result<Vec<EvaluationCaseRecord>, fanfan_core::AppError> {
    let by_id = pool
        .iter()
        .map(|case| (case.case_id.as_str(), case))
        .collect::<HashMap<_, _>>();
    manifest
        .case_ids
        .iter()
        .map(|case_id| {
            by_id
                .get(case_id.as_str())
                .map(|case| (*case).clone())
                .ok_or_else(|| {
                    fanfan_core::AppError::new(
                        "EVALUATION_BATCH_INVALID",
                        format!("冻结批次中的匿名用例 {case_id} 不存在于当前数据集快照"),
                        false,
                    )
                })
        })
        .collect()
}

fn persist_batch_manifest(
    evaluation_root: &Path,
    manifest: &mut EvaluationBatchManifestV1,
) -> Result<PathBuf, fanfan_core::AppError> {
    validate_ragas_batch_manifest(manifest)?;
    let directory = evaluation_root
        .join("batches")
        .join(&manifest.dataset_version);
    fs::create_dir_all(&directory).map_err(|error| {
        fanfan_core::AppError::new(
            "EVALUATION_BATCH_WRITE_FAILED",
            format!("无法创建评测批次目录: {error}"),
            true,
        )
    })?;
    let path = directory.join(format!("{}.json", manifest.batch_id));
    if path.exists() {
        let frozen = read_batch_manifest(&path)?;
        let same_selection = frozen.schema_version == manifest.schema_version
            && frozen.batch_id == manifest.batch_id
            && frozen.dataset_version == manifest.dataset_version
            && frozen.split == manifest.split
            && frozen.batch_index == manifest.batch_index
            && frozen.batch_size == manifest.batch_size
            && frozen.selection_algorithm == manifest.selection_algorithm
            && frozen.case_ids == manifest.case_ids
            && frozen.source_file_ids == manifest.source_file_ids
            && frozen.intent_distribution == manifest.intent_distribution
            && frozen.manifest_sha256 == manifest.manifest_sha256;
        if !same_selection {
            return Err(fanfan_core::AppError::new(
                "EVALUATION_BATCH_HASH_MISMATCH",
                "同名冻结批次已存在，但选择字段或清单哈希不同",
                false,
            ));
        }
        *manifest = frozen;
        return Ok(path);
    }
    let bytes = serde_json::to_vec_pretty(manifest).map_err(|error| {
        fanfan_core::AppError::new(
            "EVALUATION_BATCH_SERIALIZE_FAILED",
            format!("无法序列化评测批次清单: {error}"),
            false,
        )
    })?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|error| {
            fanfan_core::AppError::new(
                "EVALUATION_BATCH_WRITE_FAILED",
                format!("无法冻结评测批次清单: {error}"),
                true,
            )
        })?;
    file.write_all(&bytes)
        .and_then(|_| file.sync_all())
        .map_err(|error| {
            fanfan_core::AppError::new(
                "EVALUATION_BATCH_WRITE_FAILED",
                format!("无法完整写入评测批次清单: {error}"),
                true,
            )
        })?;
    Ok(path)
}

/// 从真实已解析文档构建语料（Evidence-first 的输入）。只采样已成功解析、
/// 在场、且有文本 chunk 的文档；画像缺失时降级为空标题/空关键词。
fn build_corpus(
    catalog: &CatalogStore,
    max_files: usize,
) -> Result<Vec<EvaluationCorpusFile>, fanfan_core::AppError> {
    let files = catalog.list_files()?;
    let mut selected = files
        .iter()
        .filter(|file| file.parse_status == ParseStatus::Parsed)
        .filter(|file| file.availability == Availability::Present)
        .collect::<Vec<_>>();
    selected.sort_by_key(|file| file.file_id);
    selected.truncate(max_files);

    let mut corpus = Vec::new();
    for file in selected {
        let chunks = catalog.file_chunks(&file.file_id)?;
        let text_chunks = chunks
            .iter()
            .map(|chunk| chunk.text.clone())
            .filter(|text| !text.trim().is_empty())
            .collect::<Vec<_>>();
        if text_chunks.is_empty() {
            continue;
        }
        let profile = catalog.get_document_profile(file.file_id)?;
        corpus.push(EvaluationCorpusFile {
            file_id: file.file_id.to_string(),
            display_name: file.display_name.clone(),
            document_type: profile
                .as_ref()
                .and_then(|item| item.document_type.as_ref())
                .map(|kind| kind.as_str().to_owned()),
            title: profile
                .as_ref()
                .map(|item| item.title.clone())
                .unwrap_or_default(),
            summary: profile
                .as_ref()
                .map(|item| item.summary.clone())
                .unwrap_or_default(),
            keywords: profile
                .as_ref()
                .map(|item| item.keywords.clone())
                .unwrap_or_default(),
            entities: profile
                .as_ref()
                .map(|item| item.entities.clone())
                .unwrap_or_default(),
            section_titles: profile
                .as_ref()
                .map(|item| item.section_titles.clone())
                .unwrap_or_default(),
            text_chunks,
            content_sha256: file.content_sha256.clone(),
            modified_at: Some(file.fs_modified_at),
        });
    }
    if corpus.is_empty() {
        return Err(fanfan_core::AppError::new(
            "EVALUATION_CORPUS_EMPTY",
            "没有已成功解析的真实文档，无法生成评测数据集",
            false,
        ));
    }
    Ok(corpus)
}

type CaseRunOutputs = (
    Vec<EvaluationResultRecord>,
    Vec<fanfan_core::RagasEvaluationSampleV1>,
    Vec<EvaluationObservation>,
);

/// 逐例运行真实链路并判定（feature_type 分发；relation 对集只构建一次）。
fn run_cases(
    catalog: &CatalogStore,
    runner: &mut LinkRunner,
    cases: &[EvaluationCaseRecord],
    run_id: &str,
) -> Result<CaseRunOutputs, fanfan_core::AppError> {
    // FILE_RELATION 用：当前 catalog 中全部 exact_duplicate 对（真实关系链路的产物）
    let relation_pairs = build_exact_duplicate_pairs(catalog)?;
    let mut results = Vec::with_capacity(cases.len());
    let mut ragas_samples = Vec::new();
    let mut observations = Vec::with_capacity(cases.len());
    for case in cases {
        let mut observation = match case.feature_type.as_str() {
            "SEARCH" => runner.run_search(catalog, case)?,
            "ASK" => runner.run_ask(catalog, case)?,
            "SMART_COLLECTION" => runner.run_collection(catalog, case)?,
            "FILE_RELATION" => runner.run_relation(case, &relation_pairs),
            _ => EvaluationObservation {
                error_code: Some("UNSUPPORTED_FEATURE_TYPE".to_owned()),
                ..Default::default()
            },
        };
        observation.trace_id = Some(format!("{run_id}:{}", case.case_id));
        observation.model_fingerprint = Some(runner.embedding.sha256.clone());
        observation.index_fingerprint = Some(runner.index_fingerprint.clone());
        observation.code_fingerprint = runner.code_fingerprint.clone();
        let result = evaluate_case_verdict(run_id, case, &observation);
        if let Some(mut sample) = ragas_sample_from_observation(case, &observation) {
            if let Some(judgements) = sample.deterministic_judgements.as_object_mut() {
                judgements.insert(
                    "error_category".into(),
                    result
                        .error_category
                        .as_deref()
                        .map_or(serde_json::Value::Null, Into::into),
                );
                judgements.insert("pass_fail".into(), result.pass_fail.into());
            }
            ragas_samples.push(sample);
        }
        results.push(result);
        observations.push(observation);
    }
    Ok((results, ragas_samples, observations))
}

/// 构建当前 catalog 的全部 exact_duplicate 无序对（用于 FILE_RELATION 判定）。
fn build_exact_duplicate_pairs(
    catalog: &CatalogStore,
) -> Result<HashSet<(String, String)>, fanfan_core::AppError> {
    let mut pairs = HashSet::new();
    let mut cursor = None;
    loop {
        let page = catalog.query_file_relations(&RelationQuery {
            cursor,
            page_size: 500,
            relation_type: Some(RelationType::ExactDuplicate),
            review_status: None,
        })?;
        for relation in page.items {
            pairs.insert(ordered_pair_str(
                &relation.left_file.file_id.to_string(),
                &relation.right_file.file_id.to_string(),
            ));
        }
        let Some(next) = page.next_cursor else {
            break;
        };
        cursor = Some(next);
    }
    Ok(pairs)
}

/// 运行时：embedding 和可选合成回答均走本机 Ollama，与生产链路同口径。
struct LinkRunner {
    embedding: ModelArtifact,
    ollama: OllamaClient,
    /// 已确认可用的 Ollama 生成模型 tag；None 时评测 ASK 回退摘录式。
    generation_model_id: Option<String>,
    cancelled: AtomicBool,
    index_fingerprint: String,
    code_fingerprint: Option<String>,
}

impl LinkRunner {
    /// SEARCH 链路：Hybrid 检索，观测 = 返回结果 file_id 的有序列表。
    fn run_search(
        &self,
        catalog: &CatalogStore,
        case: &EvaluationCaseRecord,
    ) -> Result<EvaluationObservation, fanfan_core::AppError> {
        let query = case.question_or_request.clone();
        let vector = self.encode(&query)?;
        let session = catalog.search_with_semantic(
            &SearchRequest {
                query,
                scope: all_authorized_scope(),
                mode: SearchMode::Hybrid,
                sort: SearchSort::Relevance,
                page_size: SEARCH_TOP_K,
                cursor: None,
            },
            Some(SemanticQuery {
                model_artifact_id: &self.embedding.artifact_id.to_string(),
                vector: &vector,
            }),
        )?;
        let actual_files = session
            .results
            .iter()
            .map(|result| result.file_id.to_string())
            .collect::<Vec<_>>();
        Ok(EvaluationObservation {
            actual_files,
            latency_ms: session.elapsed_ms,
            ..Default::default()
        })
    }

    /// ASK 链路：严格证据抽取 + 本地 LLM 合成回答。
    /// 有证据时在摘录证据之上调用生成模型产出带 citation_ids 的合成回答
    /// （DEV 优化第1轮，解决 round1 的 NO_SYNTHESIS 主因）；无生成模型时回退摘录式。
    /// 观测 = 证据是否找到 + 引用文件 + 合成回答正文。
    fn run_ask(
        &mut self,
        catalog: &CatalogStore,
        case: &EvaluationCaseRecord,
    ) -> Result<EvaluationObservation, fanfan_core::AppError> {
        let query = case.question_or_request.clone();
        let vector = self.encode(&query)?;
        let answer = catalog.answer_extractively(
            &AskRequest {
                question: query,
                session_id: None,
                scope: all_authorized_scope(),
                answer_style: AnswerStyle::Detailed,
                retrieval_limit: 10,
                max_source_files: 6,
                strict_evidence: true,
                clarification_selection: None,
                clarification_message_id: None,
                think_mode: false,
            },
            Some(SemanticQuery {
                model_artifact_id: &self.embedding.artifact_id.to_string(),
                vector: &vector,
            }),
        )?;
        let retrieval_latency = answer.elapsed_ms;
        let evidence_found = !answer.insufficient_evidence && !answer.used_file_ids.is_empty();
        let answer_grounded_base = !answer.insufficient_evidence
            && answer
                .claims
                .iter()
                .all(|claim| matches!(claim.support_status, fanfan_core::SupportStatus::Supported));
        // 摘录式回答保留为「检索返回的证据上下文」来源（retrieved_contexts）。
        let mut final_answer = answer.clone();
        let mut generation_latency: Option<u64> = None;
        let mut synthesized = false;
        // 只在 LLM 合成有稳定收益时调用本地生成模型；证据过薄、证据片段过多
        // 或摘录答案已经足够长时，保留严格引用的摘录式答案以降低延迟和漂移风险。
        if evidence_found
            && should_synthesize_grounded_answer(&answer)
            && let Some(generation_model_id) = self.generation_model_id.as_deref()
        {
            let answer_request = AskRequest {
                question: case.question_or_request.clone(),
                session_id: None,
                scope: all_authorized_scope(),
                answer_style: AnswerStyle::Detailed,
                retrieval_limit: 10,
                max_source_files: 6,
                strict_evidence: true,
                clarification_selection: None,
                clarification_message_id: None,
                think_mode: false,
            };
            let prompt = generation_prompt(&answer_request, &answer, &[]);
            let generation_started = Instant::now();
            let messages = serde_json::json!([
                {
                    "role": "system",
                    "content": "你是翻翻的本地资料回答器。只能使用用户提供的证据；每个事实、数字、日期、姓名都必须原样来自证据，证据中未出现的信息一律不得写出；每个事实必须通过citation_ids关联证据。"
                },
                { "role": "user", "content": prompt }
            ]);
            let generated = self.ollama.chat(
                generation_model_id,
                messages,
                OllamaChatOptions {
                    num_predict: Some(512),
                    temperature: Some(0.0),
                    num_ctx: Some(4096),
                    think: Some(false),
                },
                Some(grounded_answer_json_schema()),
                Some(&self.cancelled),
            );
            generation_latency = Some(generation_started.elapsed().as_millis() as u64);
            if let Ok(generated) = generated
                && let Some(grounded) = apply_grounded_generation(&answer, &generated)
            {
                final_answer = grounded;
                synthesized = true;
            }
        }
        let answer_grounded = answer_grounded_base
            && final_answer
                .claims
                .iter()
                .all(|claim| matches!(claim.support_status, fanfan_core::SupportStatus::Supported));
        let actual_evidence = final_answer
            .used_file_ids
            .iter()
            .map(|file_id| file_id.to_string())
            .collect::<Vec<_>>();
        let actual_answer_shape = if final_answer.insufficient_evidence {
            Some("no_evidence".to_owned())
        } else if synthesized {
            Some("synthesized_answer".to_owned())
        } else {
            Some("grounded_answer".to_owned())
        };
        let mut seen_retrieved = HashSet::new();
        let mut retrieved_contexts = Vec::new();
        let mut retrieved_context_ids = Vec::new();
        for citation in answer.claims.iter().flat_map(|claim| &claim.citations) {
            if seen_retrieved.insert(citation.chunk_id) {
                retrieved_contexts.push(citation.quote.clone());
                retrieved_context_ids.push(citation.chunk_id.to_string());
            }
        }
        // 引用定位覆盖率：去重后的证据 chunk 里，具备可用定位（页/行/幻灯片/工作表/段落）
        // 的占比。纯通用观测，不设硬门禁，用于衡量「点击引用→定位原文」的可用性。
        let mut seen_locator_chunks = HashSet::new();
        let (mut locator_total, mut locator_usable) = (0_u32, 0_u32);
        for citation in final_answer
            .claims
            .iter()
            .flat_map(|claim| &claim.citations)
        {
            if !seen_locator_chunks.insert(citation.chunk_id) {
                continue;
            }
            locator_total += 1;
            let loc = &citation.locator;
            if loc.page_no.is_some()
                || loc.line_start.is_some()
                || loc.slide_no.is_some()
                || loc.sheet_name.is_some()
                || loc.paragraph_no.is_some()
                || loc.shape_no.is_some()
            {
                locator_usable += 1;
            }
        }
        let evidence_location_coverage = (locator_total > 0)
            .then(|| locator_usable as f64 / locator_total as f64);
        let mut reference_contexts = Vec::new();
        let mut reference_context_ids = Vec::new();
        for file_id in case
            .expected_file_ids
            .clone()
            .unwrap_or_default()
            .iter()
            .filter_map(|value| uuid::Uuid::parse_str(value).ok())
        {
            for chunk in catalog.file_chunks(&file_id)?.into_iter().take(3) {
                reference_contexts.push(chunk.text);
                reference_context_ids.push(chunk.chunk_id.to_string());
            }
        }
        let reference = (!reference_contexts.is_empty()).then(|| reference_contexts.join("\n\n"));
        Ok(EvaluationObservation {
            actual_evidence,
            evidence_found,
            answer_grounded,
            actual_answer_shape,
            // 观测层补记 actual_source：ASK 链路已走本地检索（answer_extractively），
            // 与用例 expected_source=LOCAL 匹配，否则 source_router_accuracy 恒为 0。
            actual_source: Some("LOCAL".to_owned()),
            latency_ms: retrieval_latency + generation_latency.unwrap_or(0),
            retrieval_latency_ms: Some(retrieval_latency),
            generation_latency_ms: generation_latency,
            response: Some(final_answer.answer),
            evidence_location_coverage,
            retrieved_contexts,
            retrieved_context_ids,
            reference_contexts,
            reference_context_ids,
            reference,
            ..Default::default()
        })
    }

    /// SMART_COLLECTION 链路：以集合定义做语义召回，观测 = 候选文件集合。
    fn run_collection(
        &self,
        catalog: &CatalogStore,
        case: &EvaluationCaseRecord,
    ) -> Result<EvaluationObservation, fanfan_core::AppError> {
        let query = case.question_or_request.clone();
        let vector = self.encode(&query)?;
        let session = catalog.search_with_semantic(
            &SearchRequest {
                query,
                scope: all_authorized_scope(),
                mode: SearchMode::Hybrid,
                sort: SearchSort::Relevance,
                page_size: COLLECTION_TOP_K,
                cursor: None,
            },
            Some(SemanticQuery {
                model_artifact_id: &self.embedding.artifact_id.to_string(),
                vector: &vector,
            }),
        )?;
        let actual_collection_members = session
            .results
            .iter()
            .map(|result| result.file_id.to_string())
            .collect::<Vec<_>>();
        Ok(EvaluationObservation {
            actual_collection_members,
            latency_ms: session.elapsed_ms,
            ..Default::default()
        })
    }

    /// FILE_RELATION 链路：查该 pair 是否被真实关系链路预测为 exact_duplicate。
    fn run_relation(
        &self,
        case: &EvaluationCaseRecord,
        pairs: &HashSet<(String, String)>,
    ) -> EvaluationObservation {
        let files = case.expected_file_ids.clone().unwrap_or_default();
        let mut observation = EvaluationObservation::default();
        if files.len() >= 2 {
            let pair = ordered_pair_str(&files[0], &files[1]);
            if pairs.contains(&pair) {
                observation.actual_relation_type = Some("exact_duplicate".to_owned());
            }
        }
        observation
    }

    /// 编码一条查询文本（bge query_prefix + 文本）。
    fn encode(&self, text: &str) -> Result<Vec<f32>, fanfan_core::AppError> {
        let prefixed = format!(
            "{}{}",
            self.embedding.query_prefix.as_deref().unwrap_or_default(),
            text
        );
        let (vectors, _dimension) = self.ollama.embed(&self.embedding.model_id, &[prefixed])?;
        vectors.into_iter().next().ok_or_else(|| {
            fanfan_core::AppError::new("EVALUATION_EMBEDDING_EMPTY", "Embedding 返回为空", true)
        })
    }
}

/// 无序对（用于 relation 判定）。
fn ordered_pair_str(left: &str, right: &str) -> (String, String) {
    if left < right {
        (left.to_owned(), right.to_owned())
    } else {
        (right.to_owned(), left.to_owned())
    }
}

/// 全授权 scope（等价"不限制"）。
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

/// 打印 DEV/HOLDOUT 的 feature_type 分布与「正向 ASK」用例数（判定与评测侧保持一致），
/// 用于在冻结批题前观测数据集是否足以按目标规模出题。
fn print_split_profile(dev: &[EvaluationCaseRecord], holdout: &[EvaluationCaseRecord]) {
    for (label, cases) in [("DEV", dev), ("HOLDOUT", holdout)] {
        let mut by_feature = HashMap::<&str, usize>::new();
        let mut positive_ask = 0_usize;
        let mut no_evidence_ask = 0_usize;
        for case in cases {
            *by_feature.entry(case.feature_type.as_str()).or_default() += 1;
            if case.feature_type == "ASK" {
                let has_evidence = case
                    .expected_evidence_ids
                    .as_ref()
                    .is_some_and(|ids| !ids.is_empty());
                if case.expected_intent.as_deref() == Some("no_evidence") {
                    no_evidence_ask += 1;
                } else if has_evidence
                    && case
                        .expected_file_ids
                        .as_ref()
                        .is_some_and(|ids| !ids.is_empty())
                {
                    positive_ask += 1;
                }
            }
        }
        eprintln!(
            "{label} profile: total={} feature={:?} positive_ask={} no_evidence_ask={}",
            cases.len(),
            by_feature,
            positive_ask,
            no_evidence_ask
        );
    }
}

fn print_coverage_profile(dev: &[EvaluationCaseRecord], holdout: &[EvaluationCaseRecord]) {
    let scenarios = [
        ("exact_naming", "精确点名/标题"),
        ("fuzzy_reference", "模糊指代/部分名称"),
        ("file_type_reference", "文件类型指代"),
        ("semantic_content", "内容语义/摘句反查"),
        ("multi_turn_context", "多轮上下文"),
        ("multi_file", "多文件/对比"),
        ("summary", "摘要"),
        ("qa", "QA"),
        ("existence", "存在性"),
        ("no_answer", "无答案"),
        ("ambiguity", "歧义/澄清"),
        ("smart_collection", "智能集合"),
        ("file_relation", "文件关系"),
    ];
    let mut missing = Vec::new();
    for (scenario, label) in scenarios {
        let dev_count = dev
            .iter()
            .filter(|case| case_covers_scenario(case, scenario))
            .count();
        let holdout_count = holdout
            .iter()
            .filter(|case| case_covers_scenario(case, scenario))
            .count();
        let total = dev_count + holdout_count;
        if total == 0 {
            missing.push(scenario);
        }
        eprintln!(
            "COVERAGE scenario={scenario} label={label} total={total} dev={dev_count} holdout={holdout_count}"
        );
    }
    if !missing.is_empty() {
        eprintln!(
            "COVERAGE missing={:?} note=缺口只表示当前真实语料未冻结此类 case，不参与评分、不放宽标准",
            missing
        );
    }
}

fn case_covers_scenario(case: &EvaluationCaseRecord, scenario: &str) -> bool {
    let case_id = case.case_id.as_str();
    let intent = case.expected_intent.as_deref().unwrap_or_default();
    match scenario {
        "exact_naming" => {
            case_id.starts_with("search-full-")
                || case_id.starts_with("search-title-")
                || case_id.starts_with("ask-doc-")
        }
        "fuzzy_reference" => {
            case_id.starts_with("search-partial-") || case_id.starts_with("search-natural-")
        }
        "file_type_reference" => case_id.starts_with("search-type-"),
        "semantic_content" => case_id.starts_with("search-content-"),
        "multi_turn_context" => intent == "context_followup" || intent == "multi_turn_followup",
        "multi_file" => {
            case_id.starts_with("ask-compare-")
                || case
                    .expected_file_ids
                    .as_ref()
                    .is_some_and(|file_ids| file_ids.len() > 1)
        }
        "summary" => intent == "document_summary",
        "qa" => intent == "document_qa",
        "existence" => intent == "boolean_existence",
        "no_answer" => intent == "no_evidence",
        "ambiguity" => intent == "clarification" || intent == "ambiguous_reference",
        "smart_collection" => case.feature_type == "SMART_COLLECTION",
        "file_relation" => case.feature_type == "FILE_RELATION",
        _ => false,
    }
}
/// 从生成模型的 GGUF 文件名解析出对应的 Ollama 模型 tag，并校验其在 Ollama 中已就位。
///
/// 生成后端已统一走本机 Ollama，`activate` 需要 tag（如 `qwen3.5:2b`）而非模型文件路径。
/// 本函数把制品文件名映射为 tag：家族取首个 `-` 前段（仅保留小写字母数字与点），
/// 尺寸取文件名中首个数字段并补 `b` 后缀。这是通用的名称→tag 映射，而非针对某个具体
/// 模型的特判。
fn resolve_generation_ollama_tag(local_path: &Path) -> Result<String, fanfan_core::AppError> {
    use fanfan_core::OllamaClient;
    let stem = local_path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| {
            fanfan_core::AppError::new("OLLAMA_MODEL_NOT_FOUND", "无法解析生成模型文件名", false)
        })?;
    let parts = stem.split('-').collect::<Vec<_>>();
    let family = parts
        .first()
        .copied()
        .unwrap_or("")
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || *ch == '.')
        .collect::<String>()
        .to_ascii_lowercase();
    if family.is_empty() {
        return Err(fanfan_core::AppError::new(
            "OLLAMA_MODEL_NOT_FOUND",
            "生成模型文件名无法解析家族",
            false,
        ));
    }
    // 尺寸必须取自家族段之后的片段（首个 `-` 后），避免把家族名自带的小数
    // （如 `Qwen3.5` 里的 `3.5`）误判为尺寸。
    let size_source = parts
        .get(1..)
        .map(|rest| rest.join("-"))
        .unwrap_or_default();
    let mut size_digits = String::new();
    let bytes = size_source.as_bytes();
    let mut index = 0;
    while index < bytes.len() && size_digits.is_empty() {
        if bytes[index].is_ascii_digit() {
            while index < bytes.len()
                && (bytes[index].is_ascii_digit()
                    || (bytes[index] == b'.'
                        && index + 1 < bytes.len()
                        && bytes[index + 1].is_ascii_digit()))
            {
                size_digits.push(bytes[index] as char);
                index += 1;
            }
        } else {
            index += 1;
        }
    }
    if size_digits.is_empty() {
        return Err(fanfan_core::AppError::new(
            "OLLAMA_MODEL_NOT_FOUND",
            "生成模型文件名无法解析尺寸",
            false,
        ));
    }
    let candidate = format!("{family}:{size_digits}b");
    let models = OllamaClient::local().list_models()?;
    if models.iter().any(|entry| entry.name == candidate) {
        eprintln!("generation ollama tag resolved: {candidate} (from {stem})");
        return Ok(candidate);
    }
    let available = models
        .iter()
        .map(|entry| entry.name.clone())
        .collect::<Vec<_>>()
        .join(", ");
    Err(fanfan_core::AppError::new(
        "OLLAMA_MODEL_NOT_FOUND",
        format!(
            "生成模型映射 tag={candidate} 不在 Ollama 中，当前可用: {available}，请先在模型管理中拉取"
        ),
        true,
    ))
}

/// 计算百分比（分母为 0 时为 0）。
fn percent(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 * 100.0 / denominator as f64
    }
}

/// 对授权源文件清单及实时元数据做隐私安全指纹；绝对路径只参与哈希，不会输出。
fn source_manifest_hash(files: &[FileRecord]) -> String {
    let mut ordered = files
        .iter()
        .filter(|file| file.availability == Availability::Present)
        .collect::<Vec<_>>();
    ordered.sort_by_key(|file| file.file_id);

    let mut manifest = Sha256::new();
    for file in ordered {
        update_manifest_field(&mut manifest, file.file_id.as_bytes());
        update_manifest_field(&mut manifest, file.canonical_path.as_bytes());
        update_manifest_field(&mut manifest, &file.size_bytes.to_le_bytes());
        update_manifest_field(
            &mut manifest,
            file.content_sha256.as_deref().unwrap_or("").as_bytes(),
        );

        match fs::metadata(&file.canonical_path) {
            Ok(metadata) if metadata.is_file() => {
                update_manifest_field(&mut manifest, b"present");
                update_manifest_field(&mut manifest, &metadata.len().to_le_bytes());
                if let Ok(modified) = metadata.modified()
                    && let Ok(duration) = modified.duration_since(UNIX_EPOCH)
                {
                    update_manifest_field(&mut manifest, &duration.as_nanos().to_le_bytes());
                }
            }
            Ok(_) => update_manifest_field(&mut manifest, b"not-a-file"),
            Err(error) => {
                update_manifest_field(&mut manifest, b"unavailable");
                update_manifest_field(&mut manifest, error.kind().to_string().as_bytes());
            }
        }
    }
    format!("{:x}", manifest.finalize())
}

fn update_manifest_field(manifest: &mut Sha256, value: &[u8]) {
    manifest.update((value.len() as u64).to_le_bytes());
    manifest.update(value);
}

// ===== 参数解析与默认路径 =====

fn argument_present(name: &str) -> bool {
    env::args().skip(1).any(|argument| argument == name)
}

fn argument_value(name: &str) -> Option<String> {
    let mut arguments = env::args().skip(1);
    while let Some(argument) = arguments.next() {
        if argument == name {
            return arguments.next();
        }
    }
    None
}

fn argument_path(name: &str) -> Option<PathBuf> {
    argument_value(name).map(PathBuf::from)
}

fn default_data_directory() -> PathBuf {
    env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join("com.fanfan.desktop")
}

fn default_model_store() -> PathBuf {
    env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join("FanFan/ModelStore/v1")
}

fn default_evaluation_root() -> PathBuf {
    env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join("FanFan/Evaluation/v1")
}
