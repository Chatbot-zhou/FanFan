use std::{
    collections::{HashMap, HashSet},
    env,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
    time::Instant,
};

use fanfan_core::{
    AnswerStyle, AskRequest, Availability, CatalogStore, EmbeddingRequest,
    EvaluationComponentScore, EvaluationIntegritySnapshot, EvaluationRun, EvaluationSafetyGates,
    EvaluationScorecard, EvaluationSplit, LocalGenerationRuntime, ModelArtifact, ModelManager,
    ModelRegistryState, ModelRole, ParseStatus, RagEvaluationCase, RagQualityMetrics,
    RelationEvaluationCase, RelationQuery, RelationType, ScopeFilter, SearchEvaluationCase,
    SearchMode, SearchRequest, SearchSort, SemanticQuery, SpeechRecognitionRequest,
    SpeechSynthesisRequest, WorkerClient, apply_grounded_generation,
    create_encrypted_evaluation_snapshot, generation_prompt, grounded_answer_json_schema,
    inspect_runtime_log_privacy, materialize_evaluation_snapshot, persist_evaluation_run,
    score_rag_cases, score_relation_cases, score_search_cases,
};
use sha2::{Digest, Sha256};

#[derive(Debug)]
struct PrivateSearchCase {
    case_id: String,
    query: String,
    expected_file_id: uuid::Uuid,
    content_derived: bool,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("本地评测未完成: code={}", error.code);
        if let Some(details) = error.details {
            eprintln!("technical={details}");
        }
        std::process::exit(1);
    }
}

fn run() -> Result<(), fanfan_core::AppError> {
    let repository_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let data_directory = argument_path("--data-dir").unwrap_or_else(default_data_directory);
    let model_store = argument_path("--model-store").unwrap_or_else(default_model_store);
    let evaluation_root =
        argument_path("--evaluation-root").unwrap_or_else(default_evaluation_root);
    let split = argument_value("--split")
        .as_deref()
        .map(parse_split)
        .transpose()?
        .unwrap_or(EvaluationSplit::Development);

    let source_database = data_directory.join("fanfan.db");
    let snapshot = create_encrypted_evaluation_snapshot(&source_database, &evaluation_root)?;
    let working_copy = materialize_evaluation_snapshot(&snapshot)?;
    let catalog = CatalogStore::open(working_copy.path.clone())?;
    let coverage = catalog.processing_coverage_snapshot()?;
    let integrity = catalog.evaluation_integrity_snapshot()?;
    let files = catalog.list_files()?;
    let authorized_ids = files
        .iter()
        .map(|file| file.file_id)
        .collect::<HashSet<_>>();
    let source_hash_before = source_manifest_hash(&files)?;
    let log_privacy = inspect_runtime_log_privacy(&data_directory.join("logs"))?;

    let mut dataset_hasher = Sha256::new();
    let mut selected = files
        .iter()
        .filter(|file| file.parse_status == ParseStatus::Parsed)
        .filter(|file| file.availability == Availability::Present)
        .filter(|file| file_split(file.file_id) == split)
        .collect::<Vec<_>>();
    selected.sort_by_key(|file| file.file_id);
    for file in &selected {
        dataset_hasher.update(file.file_id.as_bytes());
        if let Some(revision_id) = file.current_revision_id {
            dataset_hasher.update(revision_id.as_bytes());
        }
    }
    let dataset_fingerprint = format!("{:x}", dataset_hasher.finalize());
    let mut run = EvaluationRun::start(split, dataset_fingerprint);
    run.evidence
        .insert("evaluation_snapshot_sha256".into(), snapshot.sha256.clone());
    run.evidence
        .insert("source_manifest_before".into(), source_hash_before.clone());
    let private_cases = build_search_cases(&catalog, &selected)?;

    let manager = ModelManager::open_store(&model_store)?;
    let registry = manager.registry_state()?;
    let model_payload_hash_before = model_payload_manifest_hash(&registry)?;
    run.evidence.insert(
        "model_payload_manifest_before".into(),
        model_payload_hash_before.clone(),
    );
    let model_packages_complete = registry.active_artifacts.values().all(|artifact_id| {
        registry.artifacts.iter().any(|artifact| {
            artifact.artifact_id == *artifact_id
                && artifact.status == "ready"
                && artifact.package_manifest.as_ref().is_some_and(|manifest| {
                    manifest.integrity_status == "ready"
                        && manifest.self_test_status == "ready"
                        && manifest
                            .files
                            .iter()
                            .filter(|file| file.required)
                            .all(|file| file.status == "ready")
                })
        })
    });
    let embedding = manager
        .active_artifact(ModelRole::Embedding)?
        .ok_or_else(|| {
            fanfan_core::AppError::new(
                "EVALUATION_EMBEDDING_UNAVAILABLE",
                "本地评测需要已通过完整性检查的Embedding模型",
                false,
            )
        })?;
    let tokenizer = Path::new(&embedding.local_path)
        .parent()
        .map(|parent| parent.join("tokenizer.json"))
        .filter(|path| path.is_file())
        .ok_or_else(|| {
            fanfan_core::AppError::new(
                "EVALUATION_TOKENIZER_UNAVAILABLE",
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
    let mut scored_cases = Vec::with_capacity(private_cases.len());
    let mut query_vectors = HashMap::<String, Vec<f32>>::new();
    let mut unauthorized_results = 0_u64;
    for batch in private_cases.chunks(12) {
        let texts = batch
            .iter()
            .map(|case| {
                format!(
                    "{}{}",
                    embedding.query_prefix.as_deref().unwrap_or_default(),
                    case.query
                )
            })
            .collect::<Vec<_>>();
        let response = worker.encode_embeddings(&EmbeddingRequest {
            model_path: embedding.local_path.clone(),
            tokenizer_path: Some(tokenizer.to_string_lossy().into_owned()),
            texts,
            max_length: embedding.max_length.unwrap_or(512),
            threads: 2,
        })?;
        if response.vectors.len() != batch.len() {
            return Err(fanfan_core::AppError::new(
                "EVALUATION_EMBEDDING_COUNT_MISMATCH",
                "Embedding评测返回数量与请求不一致",
                true,
            ));
        }
        for (case, vector) in batch.iter().zip(response.vectors.iter()) {
            query_vectors.insert(case.case_id.clone(), vector.clone());
            let session = catalog.search_with_semantic(
                &SearchRequest {
                    query: case.query.clone(),
                    scope: all_authorized_scope(),
                    mode: SearchMode::Hybrid,
                    sort: SearchSort::Relevance,
                    page_size: 10,
                    cursor: None,
                },
                Some(SemanticQuery {
                    model_artifact_id: &embedding.artifact_id.to_string(),
                    vector,
                }),
            )?;
            let returned_file_ids = session
                .results
                .iter()
                .map(|result| result.file_id)
                .collect::<Vec<_>>();
            unauthorized_results += returned_file_ids
                .iter()
                .filter(|file_id| !authorized_ids.contains(file_id))
                .count() as u64;
            scored_cases.push(SearchEvaluationCase {
                case_id: case.case_id.clone(),
                relevant_file_ids: vec![case.expected_file_id],
                returned_file_ids,
                elapsed_ms: session.elapsed_ms,
            });
        }
    }
    let search_metrics = score_search_cases(&scored_cases);
    let evaluate_rag = argument_present("--with-rag");
    let (rag_component, generated_content_verified) = if evaluate_rag {
        let generation = manager
            .active_artifact(ModelRole::Generation)?
            .ok_or_else(|| {
                fanfan_core::AppError::new(
                    "EVALUATION_GENERATION_UNAVAILABLE",
                    "严格RAG评测需要已通过完整性检查的本地生成模型",
                    false,
                )
            })?;
        let rag_metrics = run_rag_evaluation(
            &catalog,
            &worker,
            &private_cases,
            &query_vectors,
            &authorized_ids,
            &embedding,
            &generation,
            &repository_root,
            &data_directory,
        )?;
        let verified = rag_metrics.sample_count > 0
            && rag_metrics.citation_coverage >= 1.0
            && rag_metrics.unauthorized_rejection_rate >= 1.0
            && rag_metrics.refusal_accuracy >= 1.0
            && rag_metrics.factual_correctness >= 0.85;
        (rag_metrics.to_component_score(), verified)
    } else {
        (not_evaluated_component("strict_rag", 30.0), false)
    };
    let relation_component = if argument_present("--with-relations") {
        evaluate_relations_and_collections(&catalog, &embedding.artifact_id.to_string())?
    } else {
        not_evaluated_component("relations_and_collections", 10.0)
    };
    let media_component = if argument_present("--with-media") {
        evaluate_ocr_asr_tts(&worker, &manager)?
    } else {
        not_evaluated_component("ocr_asr_tts", 10.0)
    };
    let runtime_component = evaluate_runtime_recovery_logging(&integrity, &log_privacy);
    let source_hash_after = source_manifest_hash(&files)?;
    let model_payload_hash_after = model_payload_manifest_hash(&manager.registry_state()?)?;
    run.evidence
        .insert("source_manifest_after".into(), source_hash_after.clone());
    run.evidence.insert(
        "model_payload_manifest_after".into(),
        model_payload_hash_after.clone(),
    );

    let index_quality = 0.5 * coverage.parse_coverage
        + 0.3 * coverage.embedding_coverage
        + 0.2 * coverage.vector_coverage;
    let mut index_failures = Vec::new();
    if coverage.parse_coverage < 0.95 {
        index_failures.push("parse_coverage_below_target".into());
    }
    if coverage.embedding_coverage < 0.95 {
        index_failures.push("embedding_coverage_below_target".into());
    }
    if coverage.vector_coverage < 0.95 {
        index_failures.push("vector_coverage_below_target".into());
    }
    let components = vec![
        EvaluationComponentScore {
            component: "scan_parse_index".into(),
            earned: 20.0 * index_quality,
            maximum: 20.0,
            sample_count: coverage.discovered_files,
            metrics: HashMap::from([
                ("parse_coverage".into(), coverage.parse_coverage),
                ("embedding_coverage".into(), coverage.embedding_coverage),
                ("vector_coverage".into(), coverage.vector_coverage),
                ("failed_files".into(), coverage.failed_files as f64),
            ]),
            failure_categories: index_failures,
        },
        search_metrics.to_component_score(),
        rag_component,
        relation_component,
        media_component,
        runtime_component,
    ];
    let safety_gates = EvaluationSafetyGates {
        source_files_unchanged: source_hash_before == source_hash_after,
        authorized_scope_only: unauthorized_results == 0,
        model_packages_complete: model_packages_complete
            && model_payload_hash_before == model_payload_hash_after,
        jobs_terminal_or_recoverable: integrity.jobs_terminal_or_recoverable(),
        index_key_mapping_consistent: integrity.index_key_mapping_consistent(),
        generated_content_verified,
        logs_privacy_safe: log_privacy.passed(),
    };
    run.complete(EvaluationScorecard::from_components(
        components,
        safety_gates,
    ));
    persist_evaluation_run(&evaluation_root, &run)?;
    let scorecard = run.scorecard.as_ref().expect("completed scorecard");
    println!(
        "本地评测完成: run_id={}, split={:?}, cases={}, score={:.2}, passed={}, snapshot_bytes={}",
        run.run_id,
        split,
        scored_cases.len(),
        scorecard.score,
        scorecard.passed,
        snapshot.size_bytes
    );
    Ok(())
}

fn evaluate_ocr_asr_tts(
    worker: &WorkerClient,
    manager: &ModelManager,
) -> Result<EvaluationComponentScore, fanfan_core::AppError> {
    let mut earned = 0.0;
    let mut samples = 0_u64;
    let mut failures = Vec::new();
    let mut metrics = HashMap::new();

    let ocr_ready = if let Some(artifact) = manager.active_artifact(ModelRole::Ocr)? {
        let det = artifact_companion_path(&artifact, |name| name.contains("det"));
        let cls = artifact_companion_path(&artifact, |name| name.contains("cls"));
        let dictionary = artifact_companion_path(&artifact, |name| name.ends_with(".txt"));
        match (det, cls, dictionary) {
            (Some(det), Some(cls), Some(dictionary)) => {
                samples += 1;
                worker
                    .self_test_ocr(
                        artifact.local_path.clone(),
                        det.to_string_lossy().into_owned(),
                        cls.to_string_lossy().into_owned(),
                        dictionary.to_string_lossy().into_owned(),
                        2,
                    )
                    .is_ok_and(|result| result.status == "ready")
            }
            _ => false,
        }
    } else {
        false
    };
    if ocr_ready {
        earned += 3.0;
    } else {
        failures.push("ocr_runtime_or_package_unavailable".into());
    }
    metrics.insert("ocr_self_test_ready".into(), f64::from(ocr_ready));

    let (asr_ready, silence_safe) =
        if let Some(artifact) = manager.active_artifact(ModelRole::Asr)? {
            let tokens = artifact_companion_path(&artifact, |name| name.contains("tokens"));
            let vad = artifact_companion_path(&artifact, |name| name.contains("vad"));
            match (tokens, vad) {
                (Some(tokens), Some(vad)) => {
                    samples += 2;
                    let ready = worker
                        .self_test_asr(
                            artifact.local_path.clone(),
                            tokens.to_string_lossy().into_owned(),
                            vad.to_string_lossy().into_owned(),
                            2,
                        )
                        .is_ok_and(|result| result.status == "ready");
                    let silence_safe = ready
                        && worker
                            .recognize_speech(&SpeechRecognitionRequest {
                                model_path: artifact.local_path.clone(),
                                tokens_path: tokens.to_string_lossy().into_owned(),
                                vad_model_path: vad.to_string_lossy().into_owned(),
                                samples: vec![0.0; 16_000],
                                sample_rate: 16_000,
                                threads: 2,
                            })
                            .is_ok_and(|result| result.text.trim().is_empty());
                    (ready, silence_safe)
                }
                _ => (false, false),
            }
        } else {
            (false, false)
        };
    if asr_ready {
        earned += 1.0;
    } else {
        failures.push("asr_runtime_or_package_unavailable".into());
    }
    if silence_safe {
        earned += 1.0;
    } else {
        failures.push("asr_silence_hallucination_gate_failed".into());
    }
    metrics.insert("asr_self_test_ready".into(), f64::from(asr_ready));
    metrics.insert("asr_silence_safe".into(), f64::from(silence_safe));

    let (tts_ready, tts_generated) =
        if let Some(artifact) = manager.active_artifact(ModelRole::Tts)? {
            let tokens = artifact_companion_path(&artifact, |name| name.contains("tokens"));
            let lexicon = artifact_companion_path(&artifact, |name| name.contains("lexicon"));
            match (tokens, lexicon) {
                (Some(tokens), Some(lexicon)) => {
                    samples += 2;
                    let ready = worker
                        .self_test_tts(
                            artifact.local_path.clone(),
                            tokens.to_string_lossy().into_owned(),
                            lexicon.to_string_lossy().into_owned(),
                            2,
                        )
                        .is_ok_and(|result| result.status == "ready");
                    let generated = ready
                        && worker
                            .synthesize_speech(&SpeechSynthesisRequest {
                                model_path: artifact.local_path.clone(),
                                tokens_path: tokens.to_string_lossy().into_owned(),
                                lexicon_path: lexicon.to_string_lossy().into_owned(),
                                text: "翻翻本地语音测试".into(),
                                speaker_id: 0,
                                speed: 1.0,
                                threads: 2,
                            })
                            .is_ok_and(|result| {
                                !result.audio_base64.is_empty()
                                    && result.sample_rate > 0
                                    && result.duration_ms > 0
                            });
                    (ready, generated)
                }
                _ => (false, false),
            }
        } else {
            (false, false)
        };
    if tts_ready {
        earned += 1.0;
    } else {
        failures.push("tts_runtime_or_package_unavailable".into());
    }
    if tts_generated {
        earned += 1.0;
    } else {
        failures.push("tts_generation_failed".into());
    }
    metrics.insert("tts_self_test_ready".into(), f64::from(tts_ready));
    metrics.insert("tts_audio_generated".into(), f64::from(tts_generated));
    failures.push("ocr_asr_controlled_quality_corpus_pending".into());
    Ok(EvaluationComponentScore {
        component: "ocr_asr_tts".into(),
        earned,
        maximum: 10.0,
        sample_count: samples,
        metrics,
        failure_categories: failures,
    })
}

fn artifact_companion_path(
    artifact: &ModelArtifact,
    predicate: impl Fn(&str) -> bool,
) -> Option<PathBuf> {
    let parent = Path::new(&artifact.local_path).parent()?;
    artifact
        .package_manifest
        .as_ref()?
        .files
        .iter()
        .map(|file| file.file_name.as_str())
        .find(|name| predicate(&name.to_ascii_lowercase()))
        .map(|name| parent.join(name))
        .filter(|path| path.is_file())
}

fn evaluate_runtime_recovery_logging(
    integrity: &EvaluationIntegritySnapshot,
    log_privacy: &fanfan_core::LogPrivacyInspection,
) -> EvaluationComponentScore {
    let jobs_ok = integrity.jobs_terminal_or_recoverable();
    let index_ok = integrity.index_key_mapping_consistent();
    let authorization_ok = integrity.authorized_scope_only();
    let logs_ok = log_privacy.passed();
    let mut failures = Vec::new();
    if !jobs_ok {
        failures.push("nonrecoverable_background_jobs".into());
    }
    if !index_ok {
        failures.push("index_key_mapping_inconsistent".into());
    }
    if !authorization_ok {
        failures.push("files_outside_authorized_roots".into());
    }
    if !logs_ok {
        failures.push("runtime_log_privacy_violation".into());
    }
    failures.push("runtime_cancel_latency_not_measured".into());
    EvaluationComponentScore {
        component: "runtime_recovery_logging".into(),
        earned: (if logs_ok { 4.0 } else { 0.0 })
            + (if jobs_ok { 2.0 } else { 0.0 })
            + (if index_ok { 2.0 } else { 0.0 })
            + (if authorization_ok { 1.0 } else { 0.0 }),
        maximum: 10.0,
        sample_count: log_privacy.events_checked + integrity.active_jobs,
        metrics: HashMap::from([
            (
                "log_records_inspected".into(),
                log_privacy.events_checked as f64,
            ),
            (
                "log_privacy_violations".into(),
                log_privacy.violations as f64,
            ),
            ("active_jobs".into(), integrity.active_jobs as f64),
            (
                "stale_nonrecoverable_jobs".into(),
                integrity.stale_nonrecoverable_jobs as f64,
            ),
            (
                "inconsistent_active_vector_keys".into(),
                integrity.inconsistent_active_vector_keys as f64,
            ),
        ]),
        failure_categories: failures,
    }
}

fn evaluate_relations_and_collections(
    catalog: &CatalogStore,
    embedding_artifact_id: &str,
) -> Result<EvaluationComponentScore, fanfan_core::AppError> {
    let refresh = catalog.refresh_file_relations(20_000)?;
    let (semantic_pairs, contains_pairs) =
        catalog.refresh_semantic_file_relations(embedding_artifact_id, 20_000)?;
    let suggestion_refresh =
        catalog.refresh_collection_suggestions(embedding_artifact_id, 20_000)?;
    let files = catalog.list_files()?;
    let file_names = files
        .iter()
        .map(|file| (file.file_id, file.display_name.to_ascii_lowercase()))
        .collect::<HashMap<_, _>>();
    let authorized = files
        .iter()
        .map(|file| file.file_id)
        .collect::<HashSet<_>>();

    let mut expected_pairs = HashSet::<(uuid::Uuid, uuid::Uuid)>::new();
    let mut groups = HashMap::<String, Vec<uuid::Uuid>>::new();
    for file in &files {
        if let Some(hash) = file.content_sha256.as_ref() {
            groups.entry(hash.clone()).or_default().push(file.file_id);
        }
    }
    for group in groups.values().filter(|group| group.len() > 1) {
        for left in 0..group.len() {
            for right in (left + 1)..group.len() {
                expected_pairs.insert(ordered_pair(group[left], group[right]));
            }
        }
    }
    let mut predicted_pairs = HashSet::<(uuid::Uuid, uuid::Uuid)>::new();
    let mut cursor = None;
    loop {
        let page = catalog.query_file_relations(&RelationQuery {
            cursor,
            page_size: 500,
            relation_type: Some(RelationType::ExactDuplicate),
            review_status: None,
        })?;
        for relation in page.items {
            predicted_pairs.insert(ordered_pair(
                relation.left_file.file_id,
                relation.right_file.file_id,
            ));
        }
        let Some(next) = page.next_cursor else {
            break;
        };
        cursor = Some(next);
    }
    let relation_cases = expected_pairs
        .union(&predicted_pairs)
        .map(|pair| RelationEvaluationCase {
            case_id: format!("exact-{}-{}", pair.0, pair.1),
            expected_related: expected_pairs.contains(pair),
            predicted_related: predicted_pairs.contains(pair),
            expected_relation_type: expected_pairs
                .contains(pair)
                .then(|| "exact_duplicate".into()),
            predicted_relation_type: predicted_pairs
                .contains(pair)
                .then(|| "exact_duplicate".into()),
        })
        .collect::<Vec<_>>();
    let exact = score_relation_cases(&relation_cases);
    let exact_f1 = exact.metrics.get("f1").copied().unwrap_or(0.0);

    let suggestions =
        catalog.query_collection_suggestions(&fanfan_core::CollectionSuggestionQuery {
            cursor: None,
            page_size: 100,
            status: Some("suggested".into()),
        })?;
    let valid_suggestions = suggestions
        .items
        .iter()
        .filter(|suggestion| {
            let normalized_name = suggestion.suggested_name.trim().to_ascii_lowercase();
            !normalized_name.is_empty()
                && suggestion.members.len() >= 2
                && suggestion
                    .members
                    .iter()
                    .all(|member| authorized.contains(&member.file.file_id))
                && suggestion.members.iter().all(|member| {
                    file_names
                        .get(&member.file.file_id)
                        .is_none_or(|name| *name != normalized_name)
                })
        })
        .count();
    let suggestion_validity = if suggestions.items.is_empty() {
        0.0
    } else {
        valid_suggestions as f64 / suggestions.items.len() as f64
    };
    let all_suggestion_members_authorized = suggestions.items.iter().all(|suggestion| {
        suggestion
            .members
            .iter()
            .all(|member| authorized.contains(&member.file.file_id))
    });
    let exact_score = 4.0 * exact_f1;
    let semantic_pipeline_score = 1.0;
    let suggestion_score = 3.0 * suggestion_validity;
    let authorization_score = if all_suggestion_members_authorized {
        1.0
    } else {
        0.0
    };
    let mut failures = Vec::new();
    if relation_cases.is_empty() {
        failures.push("exact_duplicate_ground_truth_empty".into());
    }
    if exact_f1 < 1.0 {
        failures.push("exact_duplicate_quality_below_hard_gate".into());
    }
    if suggestion_validity < 0.85 {
        failures.push("collection_suggestion_structure_below_target".into());
    }
    failures.push("semantic_relation_human_ground_truth_pending".into());
    Ok(EvaluationComponentScore {
        component: "relations_and_collections".into(),
        earned: exact_score + semantic_pipeline_score + suggestion_score + authorization_score,
        maximum: 10.0,
        sample_count: relation_cases.len() as u64 + suggestions.items.len() as u64,
        metrics: HashMap::from([
            ("exact_duplicate_f1".into(), exact_f1),
            (
                "exact_duplicate_pairs".into(),
                refresh.exact_duplicate_pairs as f64,
            ),
            ("semantic_related_pairs".into(), semantic_pairs as f64),
            ("contains_or_summarizes_pairs".into(), contains_pairs as f64),
            (
                "collection_suggestions".into(),
                suggestion_refresh.created_suggestions as f64,
            ),
            ("collection_structure_validity".into(), suggestion_validity),
        ]),
        failure_categories: failures,
    })
}

fn ordered_pair(left: uuid::Uuid, right: uuid::Uuid) -> (uuid::Uuid, uuid::Uuid) {
    if left < right {
        (left, right)
    } else {
        (right, left)
    }
}

#[allow(clippy::too_many_arguments)]
fn run_rag_evaluation(
    catalog: &CatalogStore,
    worker: &WorkerClient,
    private_cases: &[PrivateSearchCase],
    query_vectors: &HashMap<String, Vec<f32>>,
    authorized_ids: &HashSet<uuid::Uuid>,
    embedding: &ModelArtifact,
    generation: &ModelArtifact,
    repository_root: &Path,
    data_directory: &Path,
) -> Result<RagQualityMetrics, fanfan_core::AppError> {
    let mut runtime = select_generation_runtime(repository_root, data_directory)?;
    let context_size = generation
        .context_length
        .unwrap_or(4_096)
        .clamp(2_048, 8_192);
    runtime.activate(&generation.local_path, context_size, 4)?;
    let cancelled = AtomicBool::new(false);
    let answer_schema = grounded_answer_json_schema();
    let content_cases = private_cases
        .iter()
        .filter(|case| case.content_derived)
        .collect::<Vec<_>>();
    let single_limit = argument_value("--rag-single-cases")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(24)
        .clamp(1, 24);
    let conversation_limit = argument_value("--rag-conversations")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(6)
        .clamp(0, 6);
    let negative_limit = argument_value("--rag-negative-cases")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(6)
        .clamp(1, 6);
    let embedding_artifact_id = embedding.artifact_id.to_string();
    let mut cases = Vec::new();

    for case in content_cases.iter().take(single_limit) {
        let Some(vector) = query_vectors.get(&case.case_id) else {
            continue;
        };
        let (evaluation, _) = evaluate_positive_rag_case(
            catalog,
            &mut runtime,
            &cancelled,
            &answer_schema,
            authorized_ids,
            &embedding_artifact_id,
            case,
            &case.query,
            &case.query,
            vector,
            None,
            &[],
            format!("rag-single-{}", case.case_id),
        );
        cases.push(evaluation);
    }

    for (conversation_index, case) in content_cases
        .iter()
        .skip(single_limit)
        .take(conversation_limit)
        .enumerate()
    {
        let Some(vector) = query_vectors.get(&case.case_id) else {
            continue;
        };
        let session_id = uuid::Uuid::now_v7();
        let (first, first_answer) = evaluate_positive_rag_case(
            catalog,
            &mut runtime,
            &cancelled,
            &answer_schema,
            authorized_ids,
            &embedding_artifact_id,
            case,
            &case.query,
            &case.query,
            vector,
            Some(session_id),
            &[],
            format!("rag-dialogue-{conversation_index}-turn-1"),
        );
        cases.push(first);
        let Some(first_answer) = first_answer else {
            continue;
        };
        let mut history = evaluation_history(session_id, &case.query, &first_answer.answer);
        for turn in 2..=3 {
            let followup = if turn == 2 {
                "这份资料中这一段还说明了什么？"
            } else {
                "把刚才提到的要点再归纳得具体一些。"
            };
            let rewritten =
                rewrite_followup_for_evaluation(&mut runtime, &cancelled, &history, followup)?;
            let vector = encode_queries(worker, embedding, &[rewritten.clone()])?
                .into_iter()
                .next()
                .ok_or_else(|| {
                    fanfan_core::AppError::new(
                        "EVALUATION_EMBEDDING_COUNT_MISMATCH",
                        "多轮RAG评测没有得到查询向量",
                        true,
                    )
                })?;
            let (evaluation, answer) = evaluate_positive_rag_case(
                catalog,
                &mut runtime,
                &cancelled,
                &answer_schema,
                authorized_ids,
                &embedding_artifact_id,
                case,
                &rewritten,
                followup,
                &vector,
                Some(session_id),
                &history,
                format!("rag-dialogue-{conversation_index}-turn-{turn}"),
            );
            cases.push(evaluation);
            let Some(answer) = answer else {
                break;
            };
            history.extend(evaluation_history(session_id, followup, &answer.answer));
        }
    }

    let negative_queries = [
        "FANFAN_EVAL_NO_EVIDENCE_7A91C2",
        "FANFAN_EVAL_NO_EVIDENCE_4D58B0",
        "FANFAN_EVAL_NO_EVIDENCE_9C23E7",
        "FANFAN_EVAL_NO_EVIDENCE_1B64F8",
        "FANFAN_EVAL_NO_EVIDENCE_6E30A5",
        "FANFAN_EVAL_NO_EVIDENCE_2F87D4",
    ];
    let negative_vectors = encode_queries(
        worker,
        embedding,
        &negative_queries[..negative_limit]
            .iter()
            .map(|value| (*value).to_owned())
            .collect::<Vec<_>>(),
    )?;
    for (index, (question, vector)) in negative_queries
        .iter()
        .take(negative_limit)
        .zip(negative_vectors.iter())
        .enumerate()
    {
        let started = Instant::now();
        let request = strict_ask_request(question, None);
        let extractive = catalog.answer_extractively(
            &request,
            Some(SemanticQuery {
                model_artifact_id: &embedding.artifact_id.to_string(),
                vector,
            }),
        )?;
        cases.push(RagEvaluationCase {
            case_id: format!("rag-negative-{index}"),
            expected_refusal: true,
            refused: extractive.insufficient_evidence,
            generated: false,
            factual_claims: 0,
            verified_claims: 0,
            unauthorized_citations: 0,
            expected_source_cited: false,
            elapsed_ms: started.elapsed().as_millis() as u64,
        });
    }
    runtime.stop();
    Ok(score_rag_cases(&cases))
}

#[allow(clippy::too_many_arguments)]
fn evaluate_positive_rag_case(
    catalog: &CatalogStore,
    runtime: &mut LocalGenerationRuntime,
    cancelled: &AtomicBool,
    answer_schema: &serde_json::Value,
    authorized_ids: &HashSet<uuid::Uuid>,
    embedding_artifact_id: &str,
    case: &PrivateSearchCase,
    retrieval_question: &str,
    answer_question: &str,
    vector: &[f32],
    session_id: Option<uuid::Uuid>,
    history: &[fanfan_core::AskMessage],
    case_id: String,
) -> (RagEvaluationCase, Option<fanfan_core::AnswerResult>) {
    let started = Instant::now();
    let retrieval_request = strict_ask_request(retrieval_question, session_id);
    let extractive = catalog.answer_extractively(
        &retrieval_request,
        Some(SemanticQuery {
            model_artifact_id: embedding_artifact_id,
            vector,
        }),
    );
    let Ok(extractive) = extractive else {
        return (failed_positive_rag_case(case_id, started), None);
    };
    if extractive.insufficient_evidence {
        return (failed_positive_rag_case(case_id, started), None);
    }
    let answer_request = strict_ask_request(answer_question, session_id);
    let prompt = generation_prompt(&answer_request, &extractive, history);
    let generated = runtime.complete_json_cancellable(
        "你是翻翻的本地资料回答器。只能使用用户提供的证据；每个事实必须通过citation_ids关联证据，不得补充外部知识。",
        &prompt,
        512,
        answer_schema,
        cancelled,
    );
    let Ok(generated) = generated else {
        return (failed_positive_rag_case(case_id, started), None);
    };
    let mut grounded = apply_grounded_generation(&extractive, &generated);
    if grounded.is_none() {
        let compact = generated.chars().take(12_000).collect::<String>();
        let repair_prompt = format!(
            "下面输出没有满足结构约束。只修复JSON结构和citation_ids，不得增加、删除或改写事实，不得引入新的S编号。只输出指定JSON对象。\n\n原输出：\n{compact}"
        );
        if let Ok(repaired) = runtime.complete_json_cancellable(
            "你是结构化引用修复器。不得引入新事实或新来源编号。",
            &repair_prompt,
            640,
            answer_schema,
            cancelled,
        ) {
            grounded = apply_grounded_generation(&extractive, &repaired);
        }
    }
    let Some(grounded) = grounded else {
        return (failed_positive_rag_case(case_id, started), None);
    };
    if catalog.validate_answer_evidence(&grounded).is_err() {
        return (failed_positive_rag_case(case_id, started), None);
    }
    let factual_claims = grounded.claims.len() as u64;
    let verified_claims = grounded
        .claims
        .iter()
        .filter(|claim| {
            if fanfan_core::claim_has_deterministic_support(
                &claim.text,
                claim.citations.iter().map(|citation| citation.quote.as_str()),
            ) {
                return true;
            }
            let evidence = claim
                .citations
                .iter()
                .enumerate()
                .map(|(index, citation)| format!("[E{}] {}", index + 1, citation.quote))
                .collect::<Vec<_>>()
                .join("\n");
            runtime
                .complete_cancellable(
                    "你是严格的中文证据核验器。判断事实句是否完全由给定原文证据支持，只输出SUPPORTED或UNSUPPORTED。",
                    &format!("事实句：{}\n\n原文证据：\n{}", claim.text, evidence),
                    32,
                    cancelled,
                )
                .is_ok_and(|value| support_verdict_is_positive(&value))
        })
        .count() as u64;
    let unauthorized_citations = grounded
        .claims
        .iter()
        .flat_map(|claim| claim.citations.iter())
        .filter(|citation| !authorized_ids.contains(&citation.file_id))
        .count() as u64;
    let expected_source_cited = grounded.claims.iter().any(|claim| {
        claim
            .citations
            .iter()
            .any(|citation| citation.file_id == case.expected_file_id)
    });
    (
        RagEvaluationCase {
            case_id,
            expected_refusal: false,
            refused: false,
            generated: true,
            factual_claims,
            verified_claims,
            unauthorized_citations,
            expected_source_cited,
            elapsed_ms: started.elapsed().as_millis() as u64,
        },
        Some(grounded),
    )
}

fn failed_positive_rag_case(case_id: String, started: Instant) -> RagEvaluationCase {
    RagEvaluationCase {
        case_id,
        expected_refusal: false,
        refused: false,
        generated: false,
        factual_claims: 0,
        verified_claims: 0,
        unauthorized_citations: 0,
        expected_source_cited: false,
        elapsed_ms: started.elapsed().as_millis() as u64,
    }
}

fn strict_ask_request(question: &str, session_id: Option<uuid::Uuid>) -> AskRequest {
    AskRequest {
        question: question.to_owned(),
        session_id,
        scope: all_authorized_scope(),
        answer_style: AnswerStyle::Detailed,
        retrieval_limit: 10,
        max_source_files: 6,
        strict_evidence: true,
    }
}

fn encode_queries(
    worker: &WorkerClient,
    embedding: &ModelArtifact,
    queries: &[String],
) -> Result<Vec<Vec<f32>>, fanfan_core::AppError> {
    let tokenizer = Path::new(&embedding.local_path)
        .parent()
        .map(|parent| parent.join("tokenizer.json"))
        .filter(|path| path.is_file())
        .ok_or_else(|| {
            fanfan_core::AppError::new(
                "EVALUATION_TOKENIZER_UNAVAILABLE",
                "Embedding模型缺少配套tokenizer",
                false,
            )
        })?;
    let texts = queries
        .iter()
        .map(|query| {
            format!(
                "{}{}",
                embedding.query_prefix.as_deref().unwrap_or_default(),
                query
            )
        })
        .collect();
    let response = worker.encode_embeddings(&EmbeddingRequest {
        model_path: embedding.local_path.clone(),
        tokenizer_path: Some(tokenizer.to_string_lossy().into_owned()),
        texts,
        max_length: embedding.max_length.unwrap_or(512),
        threads: 2,
    })?;
    if response.vectors.len() != queries.len() {
        return Err(fanfan_core::AppError::new(
            "EVALUATION_EMBEDDING_COUNT_MISMATCH",
            "Embedding评测返回数量与请求不一致",
            true,
        ));
    }
    Ok(response.vectors)
}

fn evaluation_history(
    session_id: uuid::Uuid,
    question: &str,
    answer: &str,
) -> Vec<fanfan_core::AskMessage> {
    let now = chrono::Utc::now();
    vec![
        fanfan_core::AskMessage {
            message_id: uuid::Uuid::now_v7(),
            session_id,
            role: "user".into(),
            content: question.into(),
            answer: None,
            error: None,
            created_at: now,
        },
        fanfan_core::AskMessage {
            message_id: uuid::Uuid::now_v7(),
            session_id,
            role: "assistant".into(),
            content: answer.into(),
            answer: None,
            error: None,
            created_at: now,
        },
    ]
}

fn rewrite_followup_for_evaluation(
    runtime: &mut LocalGenerationRuntime,
    cancelled: &AtomicBool,
    history: &[fanfan_core::AskMessage],
    followup: &str,
) -> Result<String, fanfan_core::AppError> {
    let history_text = history
        .iter()
        .map(|message| format!("{}：{}", message.role, message.content))
        .collect::<Vec<_>>()
        .join("\n");
    let rewritten = runtime.complete_cancellable(
        "你负责将连续追问改写为独立的中文检索问题，不得引入历史中不存在的事实。",
        &format!(
            "对话历史：\n{history_text}\n\n当前追问：{followup}\n\n把当前追问改写成可独立检索的问题。只输出改写后的问题，不回答。"
        ),
        160,
        cancelled,
    )?;
    let rewritten = rewritten.trim();
    if rewritten.is_empty() {
        return Err(fanfan_core::AppError::new(
            "EVALUATION_RAG_REWRITE_EMPTY",
            "多轮RAG评测的追问改写为空",
            true,
        ));
    }
    Ok(rewritten.chars().take(2_000).collect())
}

fn support_verdict_is_positive(value: &str) -> bool {
    value
        .split(|character: char| !character.is_ascii_alphabetic())
        .find(|token| !token.is_empty())
        .is_some_and(|token| token.eq_ignore_ascii_case("SUPPORTED"))
}

#[cfg(test)]
mod tests {
    use super::support_verdict_is_positive;

    #[test]
    fn support_verdict_requires_the_first_token_to_be_supported() {
        assert!(support_verdict_is_positive("SUPPORTED"));
        assert!(support_verdict_is_positive("SUPPORTED\n"));
        assert!(!support_verdict_is_positive("UNSUPPORTED"));
        assert!(!support_verdict_is_positive("NOT SUPPORTED"));
        assert!(!support_verdict_is_positive("The claim is SUPPORTED"));
    }
}

fn select_generation_runtime(
    repository_root: &Path,
    data_directory: &Path,
) -> Result<LocalGenerationRuntime, fanfan_core::AppError> {
    if let Some(explicit) = argument_path("--llama-runtime") {
        if explicit.is_file() {
            return Ok(LocalGenerationRuntime::new(explicit));
        }
        return Err(fanfan_core::AppError::new(
            "EVALUATION_GENERATION_RUNTIME_UNAVAILABLE",
            "指定的llama.cpp评测运行时不存在",
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
            fanfan_core::AppError::new(
                "EVALUATION_GENERATION_RUNTIME_UNAVAILABLE",
                "缺少llama.cpp CPU评测运行时",
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

fn build_search_cases(
    catalog: &CatalogStore,
    files: &[&fanfan_core::FileRecord],
) -> Result<Vec<PrivateSearchCase>, fanfan_core::AppError> {
    let mut cases = Vec::new();
    for file in files.iter().take(20) {
        let stem = Path::new(&file.display_name)
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or(&file.display_name)
            .trim();
        if stem.chars().count() >= 2 {
            cases.push(PrivateSearchCase {
                case_id: format!("filename-{}", file.file_id),
                query: stem.to_owned(),
                expected_file_id: file.file_id,
                content_derived: false,
            });
        }
    }
    for file in files {
        if cases.len() >= 60 {
            break;
        }
        let preview = catalog.file_preview(&file.file_id, 20)?;
        let Some(text) = preview
            .nodes
            .iter()
            .filter_map(|node| node.text.as_deref())
            .map(normalize_private_query_text)
            .find(|text| text.chars().count() >= 16)
        else {
            continue;
        };
        let phrase = text.chars().take(36).collect::<String>();
        cases.push(PrivateSearchCase {
            case_id: format!("content-{}", file.file_id),
            query: format!("哪里提到了{phrase}"),
            expected_file_id: file.file_id,
            content_derived: true,
        });
    }
    Ok(cases)
}

fn normalize_private_query_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn source_manifest_hash(
    files: &[fanfan_core::FileRecord],
) -> Result<String, fanfan_core::AppError> {
    let mut entries = Vec::new();
    for file in files
        .iter()
        .filter(|file| file.availability == Availability::Present)
    {
        let path = Path::new(&file.canonical_path);
        if !path.is_file() {
            continue;
        }
        let mut source = File::open(path).map_err(|_| {
            fanfan_core::AppError::new(
                "EVALUATION_SOURCE_READ_FAILED",
                "无法读取授权范围内的源文件进行只读哈希校验",
                true,
            )
        })?;
        let mut hash = Sha256::new();
        let mut buffer = vec![0_u8; 1024 * 1024];
        loop {
            let read = source.read(&mut buffer).map_err(|_| {
                fanfan_core::AppError::new(
                    "EVALUATION_SOURCE_READ_FAILED",
                    "源文件哈希校验期间读取失败",
                    true,
                )
            })?;
            if read == 0 {
                break;
            }
            hash.update(&buffer[..read]);
        }
        entries.push((file.file_id, hash.finalize().to_vec()));
    }
    entries.sort_by_key(|(file_id, _)| *file_id);
    let mut manifest = Sha256::new();
    for (file_id, hash) in entries {
        manifest.update(file_id.as_bytes());
        manifest.update(hash);
    }
    Ok(format!("{:x}", manifest.finalize()))
}

fn model_payload_manifest_hash(
    registry: &ModelRegistryState,
) -> Result<String, fanfan_core::AppError> {
    let mut artifacts = registry.artifacts.iter().collect::<Vec<_>>();
    artifacts.sort_by_key(|artifact| artifact.artifact_id);
    let mut manifest = Sha256::new();
    let mut seen = HashSet::<PathBuf>::new();
    for artifact in artifacts {
        let parent = Path::new(&artifact.local_path).parent().ok_or_else(|| {
            fanfan_core::AppError::new(
                "EVALUATION_MODEL_PAYLOAD_INVALID",
                "模型仓库中的文件位置无效",
                false,
            )
        })?;
        let files = artifact
            .package_manifest
            .as_ref()
            .map(|package| {
                package
                    .files
                    .iter()
                    .map(|file| file.file_name.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|| {
                Path::new(&artifact.local_path)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .map(|name| vec![name.to_owned()])
                    .unwrap_or_default()
            });
        for file_name in files {
            let path = parent.join(&file_name);
            if !seen.insert(path.clone()) {
                continue;
            }
            let mut file = File::open(&path).map_err(|_| {
                fanfan_core::AppError::new(
                    "EVALUATION_MODEL_PAYLOAD_MISSING",
                    "已登记模型的必要文件缺失",
                    false,
                )
            })?;
            let mut hash = Sha256::new();
            let mut size = 0_u64;
            let mut buffer = vec![0_u8; 1024 * 1024];
            loop {
                let read = file.read(&mut buffer).map_err(|_| {
                    fanfan_core::AppError::new(
                        "EVALUATION_MODEL_PAYLOAD_READ_FAILED",
                        "模型仓库完整性校验读取失败",
                        true,
                    )
                })?;
                if read == 0 {
                    break;
                }
                size = size.saturating_add(read as u64);
                hash.update(&buffer[..read]);
            }
            manifest.update(artifact.artifact_id.as_bytes());
            manifest.update(file_name.as_bytes());
            manifest.update(size.to_le_bytes());
            manifest.update(hash.finalize());
        }
    }
    Ok(format!("{:x}", manifest.finalize()))
}

fn not_evaluated_component(name: &str, maximum: f64) -> EvaluationComponentScore {
    EvaluationComponentScore {
        component: name.into(),
        earned: 0.0,
        maximum,
        sample_count: 0,
        metrics: HashMap::new(),
        failure_categories: vec!["not_evaluated".into()],
    }
}

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

fn file_split(file_id: uuid::Uuid) -> EvaluationSplit {
    match file_id.as_bytes()[15] % 10 {
        0..=5 => EvaluationSplit::Tuning,
        6..=7 => EvaluationSplit::Development,
        _ => EvaluationSplit::Hidden,
    }
}

fn parse_split(value: &str) -> Result<EvaluationSplit, fanfan_core::AppError> {
    match value {
        "tuning" => Ok(EvaluationSplit::Tuning),
        "development" | "dev" => Ok(EvaluationSplit::Development),
        "hidden" => Ok(EvaluationSplit::Hidden),
        _ => Err(fanfan_core::AppError::new(
            "EVALUATION_SPLIT_INVALID",
            "评测分组必须是tuning、development或hidden",
            false,
        )),
    }
}

fn argument_path(name: &str) -> Option<PathBuf> {
    argument_value(name).map(PathBuf::from)
}

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
