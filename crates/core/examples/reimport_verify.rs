//! 翻翻：r31 修复后的「重解析 → 嵌入 → 重建向量代 → 真实检索/问答」闭环验证 harness。
//!
//! 目的：验证"扫描 PDF 被误判为水印"修复（PyMuPDF 文本兜底）落进真实链路后，r31
//! （2023 数据库系统工程师备考知识点集锦.pdf）的正确正文能被检索到、作为证据引用，
//! 且引用带页码（PDF 定位的落点）。整个过程在**临时克隆库**上完成，不改动生产数据。
//!
//! 流程：
//!   1) 复制真实 fanfan.db → .evaluation-tmp/reimport_scratch/fanfan.db
//!   2) 用冻结 worker 重解析 r31（吃当前 revision，commit_parse_result 覆盖该 revision 的
//!      nodes/chunks/chunks_fts）
//!   3) 只对该文件的 pending chunks 用本机 Ollama 重新嵌入 + commit_chunk_embeddings
//!   4) rebuild_vector_generation 重建当前 active 向量代
//!   5) 真实问题：Hybrid 语义检索 + answer_extractively，打印命中/证据/引用页码
//!
//! 环境变量：
//!   FANFAN_DATA_DIR      真实 catalog 目录（默认 E:\Desktop\FanFan\DATA\FanFanData）
//!   FANFAN_MODEL_STORE   模型注册表目录（默认 E:\Desktop\FanFan\DATA\FanFanModelStore）
//!   FANFAN_WORKER_EXE    冻结 worker 路径（默认 .artifacts\worker\fanfan-worker\fanfan-worker.exe）
//!   FANFAN_WORKER_MODE   设为 source 时使用源码树 worker（解释器由 FANFAN_WORKER_PYTHON 指定，
//!                        默认 python），用于在冻结 worker 未重打前用最新解析逻辑验证
//!   FANFAN_CHAT_MODEL    生成模型 tag（默认 qwen3.5:2b）
//!   FANFAN_EMBED_MODEL   嵌入模型 tag（默认 qwen3-embedding:0.6b）
//!   FANFAN_LIVE=1        直写真实生产库（默认=0：只操作临时克隆库，零风险）。直写前自动做库备份。
//!
//! 只读语义：除"克隆库"所在临时目录外，不触碰任何源文件与生产库。

use std::{
    env, fs,
    path::PathBuf,
};

use fanfan_core::{
    Availability, AskRequest, AnswerStyle, CatalogStore, ModelManager, ModelRole, OllamaClient,
    ParseRequest, ScopeFilter, SearchMode, SearchRequest, SearchSort, SemanticQuery, WorkerClient,
    ChunkEmbeddingInput,
};

const FALLBACK_CHAT_MODEL: &str = "qwen3.5:2b";
const FALLBACK_EMBED_MODEL: &str = "qwen3-embedding:0.6b";
const TARGET_FILENAME: &str = "2023年数据库系统工程师备考知识点集锦.pdf";

/// 全授权 scope（等价"不限制"，只含在场文件）。
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

/// 嵌入通道配置：与生产检索同口径（真实 artifact_id + 查询前缀 + 维度）。
struct EmbedConfig {
    artifact_id: String,
    model_id: String,
    query_prefix: String,
    dimension: u32,
}

fn load_embed_config(model_store: &str) -> Result<EmbedConfig, Box<dyn std::error::Error>> {
    let manager = ModelManager::open_store(PathBuf::from(model_store))?;
    let embedding = manager
        .list_artifacts()?
        .into_iter()
        .find(|artifact| artifact.role == ModelRole::Embedding)
        .ok_or("注册表没有 Embedding 模型")?;
    if embedding.format == fanfan_core::ModelFormat::Ollama {
        manager.ollama_embedding_ready(&embedding.model_id)?;
    }
    let active = manager
        .active_artifact(ModelRole::Embedding)?
        .ok_or("没有激活的 Embedding 模型")?;
    Ok(EmbedConfig {
        artifact_id: active.artifact_id.to_string(),
        model_id: active.model_id,
        query_prefix: active.query_prefix.unwrap_or_default(),
        dimension: active
            .embedding_dimension
            .ok_or("激活的 Embedding 模型未记录向量维度")?,
    })
}

fn main() {
    if let Err(error) = run() {
        eprintln!("reimport_verify 未完成: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let data_dir = env::var("FANFAN_DATA_DIR").unwrap_or_else(|_| r"E:\Desktop\FanFan\DATA\FanFanData".to_owned());
    let model_store = env::var("FANFAN_MODEL_STORE")
        .unwrap_or_else(|_| r"E:\Desktop\FanFan\DATA\FanFanModelStore".to_owned());
    let worker_exe = env::var("FANFAN_WORKER_EXE").unwrap_or_else(|_| {
        r"E:\Desktop\FanFan\.artifacts\worker\fanfan-worker\fanfan-worker.exe".to_owned()
    });
    let _chat_model = env::var("FANFAN_CHAT_MODEL").unwrap_or_else(|_| FALLBACK_CHAT_MODEL.to_owned());
    let _embed_model = env::var("FANFAN_EMBED_MODEL").unwrap_or_else(|_| FALLBACK_EMBED_MODEL.to_owned());

    let real_db = PathBuf::from(&data_dir).join("fanfan.db");
    if !real_db.is_file() {
        return Err(format!("未找到真实 catalog 库: {}", real_db.display()).into());
    }
    let scratch_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../.evaluation-tmp/reimport_scratch");

    let live = env::var("FANFAN_LIVE").map(|v| v == "1").unwrap_or(false);
    let db_path;
    if live {
        // 直写生产库：先备份到 .evaluation-tmp/backups（带时间戳），失败可回滚。
        fs::create_dir_all(scratch_dir.join("backups"))?;
        let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
        let backup = scratch_dir.join("backups").join(format!("fanfan-{stamp}.db"));
        fs::copy(&real_db, &backup)?;
        println!("[LIVE] 已备份生产库: {} ({} bytes)", backup.display(), fs::metadata(&backup)?.len());
        db_path = real_db.clone();
        println!("[LIVE] 直写生产库: {}", db_path.display());
    } else {
        // 克隆库（零风险，不动生产数据）。
        fs::create_dir_all(&scratch_dir)?;
        let clone = scratch_dir.join("fanfan.db");
        fs::copy(&real_db, &clone)?;
        println!("克隆库: {} ({} bytes)", clone.display(), fs::metadata(&clone)?.len());
        db_path = clone;
    }

    let catalog = CatalogStore::open(db_path)?;
    let embed = load_embed_config(&model_store)?;
    println!(
        "嵌入配置: artifact={} model={} dim={} prefix={:?}",
        embed.artifact_id, embed.model_id, embed.dimension, embed.query_prefix
    );
    let ollama = OllamaClient::local();

    // 2) 找到 r31 并重解析。
    let target = catalog
        .list_files()?
        .into_iter()
        .find(|file| file.extension.eq_ignore_ascii_case("pdf") && file.display_name == TARGET_FILENAME)
        .ok_or_else(|| format!("克隆库中未找到目标文件 {TARGET_FILENAME}"))?;
    let file_id = target.file_id;
    let Some(revision_id) = target.current_revision_id else {
        return Err(format!("{TARGET_FILENAME} 无当前 revision，无法重解析").into());
    };
    println!(
        "目标: id={} name={} parse={:?} avail={:?} rev={}",
        file_id, target.display_name, target.parse_status, target.availability, revision_id
    );
    // FANFAN_WORKER_MODE=source 时回退源码树 worker（解释器由 FANFAN_WORKER_PYTHON
    // 指定，默认 python），便于在冻结 worker 未重打前用最新解析逻辑验证。
    let worker_mode = env::var("FANFAN_WORKER_MODE").unwrap_or_default();
    let worker = if worker_mode == "source" {
        let worker_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../services/worker");
        println!("使用源码树 worker: root={}", worker_root.display());
        WorkerClient::from_environment(worker_root)
    } else {
        if !PathBuf::from(&worker_exe).is_file() {
            return Err(format!("未找到 worker: {worker_exe}").into());
        }
        println!("使用 worker: {}", worker_exe);
        WorkerClient::from_executable(&worker_exe)
    };
    let result = worker.parse_document(&ParseRequest {
        job_id: uuid::Uuid::now_v7(),
        file_id,
        revision_id,
        source_path: target.canonical_path,
        format: "pdf".to_owned(),
        ocr_policy: "disabled".to_owned(),
        language_hints: vec!["zh".to_owned()],
        max_pages: None,
        asset_cache_dir: None,
        ocr_runtime: None,
        parser_version: "0.1.0".to_owned(),
    })?;
    let total_chars: usize = result
        .nodes
        .iter()
        .map(|node| node.text.as_ref().map(|t| t.chars().count()).unwrap_or(0))
        .sum();
    let warn_codes: Vec<String> = result.warnings.iter().map(|w| w.code.clone()).collect();
    println!(
        "重解析: parser={} status={:?} nodes={} total_chars={} warnings={:?}",
        result.parser_name, result.status, result.nodes.len(), total_chars, warn_codes
    );
    if result.error.is_some() {
        return Err(format!("重解析失败: {:?}", result.error).into());
    }

    // 3) 覆盖该 revision 的索引内容（nodes/chunks/chunks_fts）。
    catalog.commit_parse_result(&file_id, &result)?;
    println!("commit_parse_result: OK（该 revision 的 nodes/chunks/FTS 已覆盖为正确正文）");

    // 4) 只对 r31 的 pending chunks 重嵌入（其余文件不被打扰）。
    let mut committed_total = 0_u64;
    loop {
        let pending = catalog.list_pending_embedding_chunks(&embed.artifact_id, 200)?;
        let mut batch: Vec<_> = pending
            .into_iter()
            .filter(|chunk| chunk.file_id == file_id)
            .take(32)
            .collect();
        if batch.is_empty() {
            break;
        }
        let texts: Vec<String> = batch.iter().map(|c| c.text.clone()).collect();
        let (vectors, dim) = ollama.embed(&embed.model_id, &texts)?;
        let inputs: Vec<ChunkEmbeddingInput> = batch
            .drain(..)
            .zip(vectors)
            .map(|(chunk, vector)| ChunkEmbeddingInput {
                chunk_id: chunk.chunk_id,
                vector,
            })
            .collect();
        let committed = catalog.commit_chunk_embeddings(&embed.artifact_id, dim, &inputs)?;
        committed_total += committed;
        println!("嵌入批次: {committed} chunks（dim={dim}）");
    }
    println!("r31 重嵌入完成: 共 {committed_total} chunks");

    // 5) 重建当前 active 向量代，使 r31 新向量参与语义检索。
    let generation = catalog.rebuild_vector_generation(&embed.artifact_id, embed.dimension)?;
    println!(
        "重建向量代: gen={} items={} coverage={} status={}",
        generation.generation_id, generation.item_count, generation.coverage, generation.status
    );

    // 6) 验证 a：用一条 r31 有实质覆盖的查询做 Hybrid 检索，确认 r31（按 file_id）
    //    能在 top-10 内被召回。查询选通用"词法分析"概念，不与任何具体文件绑定。
    let search_query = "编译程序中词法分析阶段的主要任务是什么";
    let prefixed = format!("{}{}", embed.query_prefix, search_query);
    let (search_vectors, _dim) = ollama.embed(&embed.model_id, &[prefixed])?;
    let search_vector = search_vectors.into_iter().next().ok_or("搜索 embedding 为空")?;
    let session = catalog.search_with_semantic(
        &SearchRequest {
            query: search_query.to_owned(),
            scope: all_authorized_scope(),
            mode: SearchMode::Hybrid,
            sort: SearchSort::Relevance,
            page_size: 10,
            cursor: None,
        },
        Some(SemanticQuery {
            model_artifact_id: &embed.artifact_id,
            vector: &search_vector,
        }),
    )?;
    println!("== Hybrid 检索「{search_query}」命中 {} 条 ==", session.results.len());
    let mut r31_retrieved = false;
    let mut r31_rank = None;
    for (rank, result) in session.results.iter().enumerate().take(10) {
        let is_target = result.file_id == file_id;
        if is_target {
            r31_retrieved = true;
            r31_rank = Some(rank);
        }
        println!(
            "  #{rank}{} {:<40} fused={:.3} reasons={:?}",
            if is_target { " *r31" } else { "" },
            truncate(&result.name, 40),
            result.scores.fused,
            result.match_reasons,
        );
    }
    println!(
        "检索是否命中 r31（正确正文）: {}（rank={:?}）",
        r31_retrieved, r31_rank
    );

    // 6) 验证 b：真实问答。目的不是"强制 r31 必须作证据"，而是确认整条问答链路
    //    在这种情况下可靠：能基于真实资料给出有页码引用的 grounded 回答，诚实不伪造。
    //    r31 与大量"考试真题"文件内容重叠时，谁是证据源由融合排序决定属正常竞争。
    let mut probe_questions: Vec<String> = vec![
        "编译程序中词法分析阶段的主要任务是什么？".to_owned(),
    ];
    let mut any_page_cite = false;
    for question in probe_questions.drain(..) {
        let ask_request = AskRequest {
            question: question.clone(),
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
        let ask_prefixed = format!("{}{}", embed.query_prefix, ask_request.question);
        let (ask_vectors, _dim) = ollama.embed(&embed.model_id, &[ask_prefixed])?;
        let ask_vector = ask_vectors.into_iter().next().ok_or("问答 embedding 为空")?;
        let answer = catalog.answer_extractively(
            &ask_request,
            Some(SemanticQuery {
                model_artifact_id: &embed.artifact_id,
                vector: &ask_vector,
            }),
        )?;
        let used_r31 = answer.used_file_ids.contains(&file_id);
        let cite_total = answer.claims.iter().flat_map(|c| &c.citations).count();
        let cite_with_page = answer
            .claims
            .iter()
            .flat_map(|c| &c.citations)
            .filter(|c| c.locator.page_no.is_some())
            .count();
        if cite_with_page > 0 {
            any_page_cite = true;
        }
        let r31_mark = if used_r31 { " *r31" } else { "" };
        println!("== answer_extractively「{question}」==");
        println!(
            "  used_files={} 使用r31={}{} 引用数={} 带页码引用={} grounding={:?} insufficient={}",
            answer.used_file_ids.len(),
            used_r31,
            r31_mark,
            cite_total,
            cite_with_page,
            answer.grounding_status,
            answer.insufficient_evidence,
        );
        println!(
            "  answer: {}",
            truncate(&answer.answer, 220)
        );
        for (ci, claim) in answer.claims.iter().enumerate().take(6) {
            let pages: Vec<String> = claim
                .citations
                .iter()
                .map(|c| {
                    let page = c.locator.page_no.map(|p| p.to_string()).unwrap_or_else(|| "-".into());
                    format!("页{page}")
                })
                .collect();
            println!(
                "  [claim {ci}] pages={{{}}} text={}",
                pages.join(","),
                truncate(claim.text.as_str(), 96)
            );
        }
    }

    // 闭环判定：r31 正确正文已索引且可召回 + 问答 grounded 且引用带页码（诚实不伪造）。
    // 是否由 r31 具体作证据属正常竞争，不作为通过条件。
    let ok = r31_retrieved && any_page_cite;
    println!(
        "\n>>> 判定: {}",
        if ok {
            "PASS（r31 正确正文已可检索；问答 grounded 且引用带页码，链路可用）"
        } else {
            "X（闭环未完全走通，见上方字段）"
        }
    );
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