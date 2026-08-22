//! 用本机 Ollama 跑完整 RAG 链路：生成（qwen3.5:2b）+ 嵌入（qwen3-embedding:0.6b），
//! Worker 提供 ONNX Rerank，检索走与生产 `run_retrieval_answer` 一致的
//! filename + FTS + embedding + RRF + MMR 混合检索。
//!
//! 流程：意图路由（LLM 直路由）→ 查询改写 → 混合检索 → Rerank →
//! 受约束生成 → 引用校验/结构修复 → 逐条证据核验 → 组装带引用的答案。
//!
//! 与生产差异（不影响答案内容）：
//! - 直接打开 `.evaluation-tmp/data-dir/fanfan.db` 工作副本，不写入真实用户库；
//! - 单轮问答（无会话历史、无图片重分析）；
//! - 不写 trace_node 遥测。
//!
//! 用法：
//! ```text
//! cargo run --release --example ask_local_ollama -- \
//!   --question "《2015年上半年数据库系统工程师考试上午真题（参考答案）.pdf》中，软件的安全需求被划分为哪几类？"
//! ```

use std::{
    collections::HashSet,
    env,
    path::PathBuf,
    sync::atomic::AtomicBool,
    time::Instant,
};

use fanfan_core::{
    AnswerStyle, AskRequest, Availability, CatalogStore, Intent, LocalGenerationRuntime,
    ModelArtifact, ModelFormat, ModelManager, ModelRole, RerankRequest, ScopeFilter, SemanticQuery,
    WorkerClient, apply_grounded_generation, chat_prompt, claim_has_deterministic_support,
    generation_prompt, grounded_answer_json_schema, intent_routing_prompt, parse_intent_verdict,
    parse_rewritten_queries, query_rewrite_prompt,
};
use fanfan_core::ollama::OllamaClient;

/// 证据门控阈值（与生产 app_data.rs 的 RERANK_CHAT_FALLBACK_THRESHOLD 一致）。
const RERANK_CHAT_FALLBACK_THRESHOLD: f32 = 0.1;

/// 入口：解析参数、打开工作副本、跑完整链路并打印结果。
fn main() {
    if let Err(error) = run() {
        eprintln!("完整链路未完成: code={} message={}", error.code, error.message);
        if let Some(details) = error.details {
            eprintln!("technical={details}");
        }
        std::process::exit(1);
    }
}

fn run() -> Result<(), fanfan_core::AppError> {
    let repository_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let data_directory = argument_path("--data-dir").unwrap_or_else(|| {
        repository_root.join(".evaluation-tmp/data-dir")
    });
    let model_store = argument_path("--model-store")
        .unwrap_or_else(|| repository_root.join(".evaluation-tmp/model-store/v1"));
    let evaluation_root = argument_path("--evaluation-root")
        .unwrap_or_else(|| repository_root.join(".evaluation-tmp/eval-root"));
    let question = argument_value("--question").unwrap_or_else(|| {
        "《2015年上半年数据库系统工程师考试上午真题（参考答案）.pdf》中，软件的安全需求被划分为哪几类？".into()
    });

    // ---------- 数据库工作副本（快照，不污染真实库） ----------
    let source_database = data_directory.join("fanfan.db");
    let snapshot =
        fanfan_core::create_encrypted_evaluation_snapshot(&source_database, &evaluation_root)?;
    let working_copy = fanfan_core::materialize_evaluation_snapshot(&snapshot)?;
    println!(
        "[setup] 数据快照已就绪: {} ({} bytes, sha256={})",
        working_copy.path.display(),
        snapshot.size_bytes,
        &snapshot.sha256[..12]
    );
    let catalog = CatalogStore::open(working_copy.path.clone())?;

    // ---------- 模型 ----------
    let manager = ModelManager::open_store(&model_store)?;
    // Ollama 注册表同步：refresh_package_manifests 会把 Ollama artifact 降级为
    // incomplete，这里探测本机已驻留的 Ollama 模型并重新登记就绪（与生产
    // ensure_ollama_registry_synced 逻辑一致），否则 active_artifact 判为 None。
    sync_ollama_registry(&manager)?;
    // 临时调试：打印注册表加载状态
    if std::env::var("ASK_DEBUG").is_ok() {
        // 通过公开 API 无法直接读注册表，仅打印 active_artifact 判定结果
        for role in [ModelRole::Embedding, ModelRole::Generation, ModelRole::Reranker] {
            println!(
                "[debug] active_artifact({:?}) = {:?}",
                role,
                manager
                    .active_artifact(role)
                    .map(|a| a.map(|x| x.model_id))
                    .unwrap_or_else(|e| Some(format!("ERROR {}", e.code)))
            );
        }
    }
    let embedding = manager
        .active_artifact(ModelRole::Embedding)?
        .ok_or_else(|| {
            fanfan_core::AppError::new(
                "ASK_EMBEDDING_UNAVAILABLE",
                "需要已通过完整性检查的 Embedding 模型",
                false,
            )
        })?;
    let generation = manager
        .active_artifact(ModelRole::Generation)?
        .ok_or_else(|| {
            fanfan_core::AppError::new(
                "ASK_GENERATION_UNAVAILABLE",
                "需要已通过完整性检查的生成模型",
                false,
            )
        })?;
    let reranker = manager.active_artifact(ModelRole::Reranker)?;
    println!(
        "[setup] embedding={} generation={} reranker={}",
        embedding.model_id,
        generation.model_id,
        reranker
            .as_ref()
            .map(|r| r.model_id.as_str())
            .unwrap_or("(none)")
    );

    // ---------- worker（ONNX Rerank） ----------
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

    // ---------- 生成运行时（Ollama 承载，qwen3.5:2b） ----------
    let mut runtime = LocalGenerationRuntime::new(PathBuf::new());
    let threads = interactive_inference_threads();
    runtime.activate(&generation.local_path, 4096, threads)?;
    println!(
        "[setup] 生成运行时已激活: {} (context=4096, threads={})",
        generation.local_path, threads
    );

    let cancelled = AtomicBool::new(false);
    let maintenance = catalog.maintenance_snapshot()?;
    let scope = all_authorized_scope();

    // ---------- 1. 意图路由（LLM 直路由，与生产一致） ----------
    let routing_started = Instant::now();
    let (system, user) = intent_routing_prompt(question.trim(), &[]);
    let intent = runtime
        .complete_cancellable(&system, &user, 32, &cancelled)
        .ok()
        .and_then(|raw| parse_intent_verdict(&raw))
        .unwrap_or(Intent::Chat);
    println!(
        "[intent] {:?} ({:.1}s)",
        intent,
        routing_started.elapsed().as_secs_f64()
    );

    // ---------- 2. 分支执行（闲聊 / 检索增强生成） ----------
    let request = AskRequest {
        question: question.trim().to_owned(),
        session_id: None,
        scope: scope.clone(),
        answer_style: AnswerStyle::Detailed,
        retrieval_limit: 10,
        max_source_files: 6,
        strict_evidence: true,
        clarification_selection: None,
        think_mode: false,
    };
    let answer = match intent {
        Intent::Chat => run_chat_branch(&request, &mut runtime, &maintenance, &cancelled)?,
        _ => run_retrieval_branch(
            &request,
            &catalog,
            &worker,
            &mut runtime,
            &embedding,
            reranker.clone(),
            &maintenance,
            &cancelled,
        )?,
    };

    runtime.stop();

    print_result(&question, &intent, &answer);
    Ok(())
}

/// 复刻生产 `ensure_ollama_registry_synced`：探测本机 Ollama 已驻留模型并登记
/// 就绪，让 active_artifact 能判到 Generation / Embedding 角色。
fn sync_ollama_registry(manager: &ModelManager) -> Result<(), fanfan_core::AppError> {
    let Some(plan) = fanfan_core::resolve_runtime_model_plan("smooth") else {
        return Ok(());
    };
    let client = OllamaClient::local();
    let Ok(local_tags) = client.list_models() else {
        // Ollama 不可用时静默跳过，不阻塞检索主链。
        return Ok(());
    };
    let local_tag_set = local_tags
        .iter()
        .map(|model| model.name.as_str())
        .collect::<HashSet<_>>();
    let report = manager.plan_preset("smooth")?;
    let generation_ready = report
        .ready
        .iter()
        .any(|item| item.role == ModelRole::Generation);
    let embedding_ready = report
        .ready
        .iter()
        .any(|item| item.role == ModelRole::Embedding);
    if !generation_ready
        && let Some(tag) = fanfan_core::ollama_tag_for_catalog(&plan.generation)
        && local_tag_set.contains(tag.as_str())
    {
        let _ = manager.ollama_generation_ready(&tag, &plan.generation);
    }
    if !embedding_ready
        && let Some(tag) = fanfan_core::ollama_tag_for_catalog(&plan.embedding)
        && local_tag_set.contains(tag.as_str())
    {
        let _ = manager.ollama_embedding_ready(&tag);
    }
    // 收敛 active_artifacts，让后续 active_artifact 能判 ready。
    let _ = manager.apply_runtime_plan("smooth");
    Ok(())
}

/// 与生产 run_chat_answer 一致（闲聊分支）。
fn run_chat_branch(
    request: &AskRequest,
    runtime: &mut LocalGenerationRuntime,
    maintenance: &fanfan_core::MaintenanceSnapshot,
    cancelled: &AtomicBool,
) -> Result<fanfan_core::AnswerResult, fanfan_core::AppError> {
    if maintenance.degradation_level == "core" {
        return Err(fanfan_core::AppError::new(
            "RAG_RESOURCE_PRESSURE",
            "当前资源压力较高，暂未启动回答；请稍后重试",
            true,
        ));
    }
    let (system, user) = chat_prompt(request, &[]);
    let started_at = Instant::now();
    let answer = runtime.complete_cancellable(&system, &user, 512, cancelled)?;
    Ok(fanfan_core::AnswerResult {
        session_id: uuid::Uuid::now_v7(),
        message_id: uuid::Uuid::now_v7(),
        answer: answer.trim().to_owned(),
        grounding_status: fanfan_core::GroundingStatus::Insufficient,
        insufficient_evidence: false,
        claims: Vec::new(),
        source_files: Vec::new(),
        used_file_ids: Vec::new(),
        elapsed_ms: started_at.elapsed().as_millis() as u64,
        answer_mode: fanfan_core::AnswerMode::Chat,
        retrieval_channels: Vec::new(),
        index_coverage: 0.0,
        degradation_reason: None,
        clarification: None,
        no_evidence_reason: None,
        thinking: None,
    })
}

/// 合并多个子查询的检索结果（镜像生产 merge_extractive_results）。
fn merge_extractive_results(
    mut results: Vec<fanfan_core::AnswerResult>,
) -> fanfan_core::AnswerResult {
    let mut merged = results.remove(0);
    for mut other in results {
        let mut seen = std::collections::HashSet::new();
        for claim in &merged.claims {
            seen.insert((
                claim.text.clone(),
                claim.citations.first().map(|citation| citation.file_id),
            ));
        }
        for claim in other.claims.drain(..) {
            let key = (
                claim.text.clone(),
                claim.citations.first().map(|citation| citation.file_id),
            );
            if seen.insert(key) {
                merged.claims.push(claim);
            }
        }
        merged.insufficient_evidence = merged.insufficient_evidence && other.insufficient_evidence;
        merged.elapsed_ms = merged.elapsed_ms.saturating_add(other.elapsed_ms);
        merged.source_files.extend(other.source_files);
    }
    merged
}

/// 与生产 run_retrieval_answer 的完整流程一致：改写 → Ollama 嵌入 →
/// 混合检索 → 证据不足拒绝 → Rerank → 受约束生成 → 引用校验 →
/// 逐条证据核验 → 组装答案。
#[allow(clippy::too_many_arguments)]
fn run_retrieval_branch(
    request: &AskRequest,
    catalog: &CatalogStore,
    worker: &WorkerClient,
    runtime: &mut LocalGenerationRuntime,
    embedding: &ModelArtifact,
    reranker: Option<ModelArtifact>,
    maintenance: &fanfan_core::MaintenanceSnapshot,
    cancelled: &AtomicBool,
) -> Result<fanfan_core::AnswerResult, fanfan_core::AppError> {
    let index_coverage = catalog
        .semantic_index_coverage(&request.scope, &embedding.artifact_id.to_string())?
        .1;
    if index_coverage <= 0.0 {
        return Err(fanfan_core::AppError::new(
            "RAG_SEMANTIC_INDEX_REQUIRED",
            "当前检索范围尚未建立语义索引，完整 RAG 已停止",
            true,
        ));
    }
    if maintenance.degradation_level == "core" {
        return Err(fanfan_core::AppError::new(
            "RAG_RESOURCE_PRESSURE",
            "当前资源压力较高，完整 RAG 暂未启动；请稍后重试",
            true,
        ));
    }

    // 改写（无历史；解析失败/为空回退原始问题）
    let rewritten_queries = {
        let (system, user) = query_rewrite_prompt(request.question.trim(), &[]);
        let rewritten = runtime
            .complete_cancellable(&system, &user, 160, cancelled)
            .unwrap_or_default();
        parse_rewritten_queries(&rewritten)
    };
    let retrieval_questions = if rewritten_queries.is_empty() {
        vec![request.question.trim().to_owned()]
    } else {
        rewritten_queries
    };
    println!("[rewrite] {:?}", retrieval_questions);

    // Ollama 嵌入：批量编码检索问题（与生产 run_embedding 一致）
    let embedding_texts = retrieval_questions
        .iter()
        .map(|question| {
            format!(
                "{}{}",
                embedding.query_prefix.as_deref().unwrap_or(""),
                question
            )
        })
        .collect::<Vec<_>>();
    let embedding_started = Instant::now();
    let client = OllamaClient::local();
    let (vectors, _dimension) = client.embed(
        fanfan_core::model_catalog::OLLAMA_EMBEDDING_TAG,
        &embedding_texts,
    )?;
    if vectors.len() != retrieval_questions.len() {
        return Err(fanfan_core::AppError::new(
            "EMBEDDING_EMPTY",
            "Ollama 嵌入没有返回全部查询向量",
            true,
        ));
    }
    println!(
        "[embed] {:.1}s ({} 段, dim={})",
        embedding_started.elapsed().as_secs_f64(),
        vectors.len(),
        vectors.first().map(|v| v.len()).unwrap_or(0)
    );
    let artifact_id = embedding.artifact_id.to_string();

    // 混合检索：filename + FTS + embedding + RRF + MMR
    let retrieval_started = Instant::now();
    let mut sub_results = Vec::with_capacity(retrieval_questions.len());
    for (question, vector) in retrieval_questions.iter().zip(vectors.iter()) {
        let mut sub_request = request.clone();
        sub_request.question = question.clone();
        sub_request.retrieval_limit = sub_request.retrieval_limit.min(10);
        sub_request.max_source_files = sub_request.max_source_files.min(6);
        sub_results.push(catalog.answer_extractively(
            &sub_request,
            Some(SemanticQuery {
                model_artifact_id: &artifact_id,
                vector,
            }),
        )?);
    }
    let mut extractive = merge_extractive_results(sub_results);
    extractive.index_coverage = index_coverage;
    extractive.retrieval_channels = vec![
        "filename".into(),
        "fts".into(),
        "embedding".into(),
        "rrf".into(),
        "mmr".into(),
    ];
    println!(
        "[retrieval] {:.1}s, 候选 {} 条, insufficient={}",
        retrieval_started.elapsed().as_secs_f64(),
        extractive.claims.len(),
        extractive.insufficient_evidence
    );
    if extractive.insufficient_evidence {
        extractive.answer_mode = fanfan_core::AnswerMode::RagRefusal;
        return Ok(extractive);
    }

    // Rerank（维护级别 full + Onnx reranker 时应用）
    if maintenance.degradation_level == "full"
        && let Some(reranker) = reranker
        && reranker.format == ModelFormat::Onnx
        && !extractive.claims.is_empty()
    {
        let rerank_tokenizer = PathBuf::from(&reranker.local_path)
            .parent()
            .map(|parent| parent.join("tokenizer.json"));
        if let Some(rerank_tokenizer) = rerank_tokenizer.filter(|path| path.is_file()) {
            let documents = extractive
                .claims
                .iter()
                .map(|claim| compact_for_prompt(&claim.text, 12_000))
                .collect::<Vec<_>>();
            if let Ok(response) = worker.rerank(&RerankRequest {
                model_path: reranker.local_path,
                tokenizer_path: Some(rerank_tokenizer.to_string_lossy().into_owned()),
                // rerank 一律用用户原始问题排序（改写只服务检索召回，与生产一致）
                query: request.question.trim().to_owned(),
                documents,
                max_length: reranker.max_length.unwrap_or(512),
                threads: 2,
            }) && apply_rerank_scores(&mut extractive, &response.scores).is_ok()
            {
                // 证据门控：top-1 分数过低 → 转闲聊（与生产一致）
                let top_score = extractive
                    .claims
                    .first()
                    .and_then(|claim| claim.citations.first())
                    .map(|citation| citation.retrieval_score)
                    .unwrap_or(0.0);
                println!("[rerank] top-1 score={top_score:.3}");
                if top_score < RERANK_CHAT_FALLBACK_THRESHOLD {
                    return run_chat_branch(request, runtime, maintenance, cancelled);
                }
                extractive.retrieval_channels.push("reranker".into());
            }
        }
    }

    // 受约束生成（grounded_answer_json_schema，与生产一致）
    let generation_started = Instant::now();
    let prompt = generation_prompt(request, &extractive, &[]);
    let mut generated = runtime.complete_json_cancellable(
        "你是翻翻的本地资料回答器。只能使用用户提供的证据；每个事实必须通过citation_ids关联证据，不得补充外部知识。",
        &prompt,
        768,
        &grounded_answer_json_schema(),
        cancelled,
    )?;
    println!(
        "[generate] {:.1}s",
        generation_started.elapsed().as_secs_f64()
    );
    if std::env::var("ASK_DEBUG").is_ok() {
        println!(
            "[debug] generated raw (前2000字符):\n{}",
            compact_for_prompt(&generated, 2000)
        );
    }

    // 引用校验 + 结构修复
    let mut grounded = apply_grounded_generation(&extractive, &generated);
    if grounded.is_none() {
        let repair_prompt = format!(
            "下面的输出没有满足结构约束。只修复JSON结构和citation_ids，不得增加、删除或改写事实，不得引入新的S编号。只输出符合指定JSON Schema的对象。\n\n原输出：\n{}",
            compact_for_prompt(&generated, 12_000)
        );
        if let Ok(repaired) = runtime.complete_json_cancellable(
            "你是结构化引用修复器。不得引入新事实或新来源编号。",
            &repair_prompt,
            640,
            &grounded_answer_json_schema(),
            cancelled,
        ) {
            grounded = apply_grounded_generation(&extractive, &repaired);
            if grounded.is_some() {
                generated = repaired;
            }
        }
    }
    let Some(mut grounded) = grounded else {
        let fallback_text = fanfan_core::extract_unverified_text(&generated);
        if fallback_text.is_empty() {
            return Err(fanfan_core::AppError::new(
                "RAG_CITATION_VALIDATION_FAILED",
                "本次生成结果未通过引用核验且没有可展示的内容，可以重试",
                true,
            ));
        }
        return Ok(fanfan_core::unverified_answer(
            &extractive,
            extractive.session_id,
            fallback_text,
            extractive.elapsed_ms,
        ));
    };

    // 逐条证据核验（确定性支持或生成模型 SUPPORTED 判定）
    let candidates = std::mem::take(&mut grounded.claims);
    let mut rejected_claims = 0_usize;
    for claim in candidates {
        let evidence = claim
            .citations
            .iter()
            .enumerate()
            .map(|(index, citation)| format!("[E{}] {}", index + 1, citation.quote))
            .collect::<Vec<_>>()
            .join("\n");
        let deterministically_supported = claim_has_deterministic_support(
            &claim.text,
            claim
                .citations
                .iter()
                .map(|citation| citation.quote.as_str()),
        );
        let supported = if deterministically_supported {
            true
        } else {
            let verification = runtime.complete_cancellable(
                "你是严格的中文证据核验器。判断事实句是否完全由给定原文证据支持，只输出SUPPORTED或UNSUPPORTED。",
                &format!("事实句：{}\n\n原文证据：\n{}", claim.text, evidence),
                32,
                cancelled,
            )?;
            claim_support_is_verified(&verification)
        };
        if !supported {
            rejected_claims = rejected_claims.saturating_add(1);
            continue;
        }
        let mut single_claim_result = grounded.clone();
        single_claim_result.claims = vec![claim.clone()];
        catalog.validate_answer_evidence(&single_claim_result)?;
        grounded.claims.push(claim);
    }
    if grounded.claims.is_empty() {
        let fallback_text = fanfan_core::extract_unverified_text(&generated);
        if fallback_text.is_empty() {
            return Err(fanfan_core::AppError::new(
                "RAG_CLAIM_UNSUPPORTED",
                "生成内容没有任何事实句通过原文支持性校验，且没有可展示的内容，回答已拒绝显示",
                true,
            ));
        }
        return Ok(fanfan_core::unverified_answer(
            &extractive,
            extractive.session_id,
            fallback_text,
            extractive.elapsed_ms,
        ));
    }

    // 组装答案 + 清理未验证来源
    grounded.answer = grounded
        .claims
        .iter()
        .map(|claim| claim.text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    let verified_file_ids = grounded
        .claims
        .iter()
        .flat_map(|claim| claim.citations.iter().map(|citation| citation.file_id))
        .collect::<HashSet<_>>();
    grounded
        .source_files
        .retain(|source| verified_file_ids.contains(&source.file_id));
    grounded.used_file_ids = verified_file_ids.into_iter().collect();
    if rejected_claims > 0 {
        grounded.grounding_status = fanfan_core::GroundingStatus::Partial;
        grounded.degradation_reason = Some(format!(
            "有{rejected_claims}个候选事实句未通过原文支持性校验，已自动隐藏"
        ));
    }
    grounded.index_coverage = index_coverage;
    grounded.retrieval_channels = extractive.retrieval_channels;
    catalog.validate_answer_evidence(&grounded)?;
    Ok(grounded)
}

/// 与生产 apply_rerank_scores 一致：按分数降序重排证据，分数写入每条引文。
fn apply_rerank_scores(
    result: &mut fanfan_core::AnswerResult,
    scores: &[f32],
) -> Result<(), fanfan_core::AppError> {
    if scores.len() != result.claims.len() || scores.iter().any(|score| !score.is_finite()) {
        return Err(fanfan_core::AppError::new(
            "RERANK_OUTPUT_INVALID",
            "重排分数数量或数值无效，已保留融合检索顺序",
            false,
        ));
    }
    let mut ranked = result
        .claims
        .drain(..)
        .zip(scores.iter().copied())
        .collect::<Vec<_>>();
    for (claim, score) in &mut ranked {
        for citation in &mut claim.citations {
            citation.retrieval_score = *score;
        }
    }
    ranked.sort_by(|left, right| right.1.total_cmp(&left.1));
    result.claims = ranked.into_iter().map(|(claim, _)| claim).collect();
    Ok(())
}

/// 与生产 claim_support_is_verified 一致：首个 ASCII 字母 token 为 SUPPORTED。
fn claim_support_is_verified(value: &str) -> bool {
    value
        .split(|character: char| !character.is_ascii_alphabetic())
        .find(|token| !token.is_empty())
        .is_some_and(|token| token.eq_ignore_ascii_case("SUPPORTED"))
}

/// 与生产 compact_for_prompt 一致：空白归一化后按字符截断。
fn compact_for_prompt(value: &str, limit: usize) -> String {
    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    normalized.chars().take(limit).collect()
}

/// 与生产一致：空过滤条件 + 仅 present 可用文件（模拟"全部授权资料"范围）。
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

/// 与生产 interactive_inference_threads 一致：(物理核数/2).clamp(1,4)。
fn interactive_inference_threads() -> u32 {
    (physical_core_count() / 2).clamp(1, 4)
}

fn physical_core_count() -> u32 {
    std::thread::available_parallelism()
        .map(|value| value.get() as u32)
        .unwrap_or(2)
        .max(1)
}

/// 打印最终问题与链路回复。
fn print_result(question: &str, intent: &Intent, answer: &fanfan_core::AnswerResult) {
    println!("\n========== 完整链路结果 ==========");
    println!("问题：{question}");
    println!("路由：{:?}  |  回答模式：{:?}", intent, answer.answer_mode);
    println!(
        "引用状态：{:?}  |  证据不足：{}  |  索引覆盖：{:.1}%  |  耗时：{}ms",
        answer.grounding_status,
        answer.insufficient_evidence,
        answer.index_coverage * 100.0,
        answer.elapsed_ms
    );
    if let Some(reason) = &answer.degradation_reason {
        println!("降级原因：{reason}");
    }
    println!("\n---- 回复 ----\n{}", answer.answer);
    if !answer.claims.is_empty() {
        println!("\n---- 证据（引用）----");
        for (index, claim) in answer.claims.iter().enumerate() {
            println!("\n[S{}] {}", index + 1, claim.text);
            for citation in &claim.citations {
                println!(
                    "     └─ 引用 {} | 文件 {} | 得分 {:.3} | 原文：{}",
                    citation.evidence_id,
                    citation.file_id,
                    citation.retrieval_score,
                    compact_for_prompt(&citation.quote, 160)
                );
            }
        }
    }
    if !answer.source_files.is_empty() {
        println!("\n---- 来源文件 ----");
        for source in &answer.source_files {
            println!("  {}  |  {}", source.display_name, source.canonical_path);
        }
    }
}

fn argument_path(name: &str) -> Option<PathBuf> {
    argument_value(name).map(PathBuf::from)
}

fn argument_value(name: &str) -> Option<String> {
    let arguments = env::args().collect::<Vec<_>>();
    arguments
        .iter()
        .position(|argument| argument == name)
        .and_then(|index| arguments.get(index + 1).cloned())
}
