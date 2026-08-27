//! 翻翻：验证 PDF bbox 回填后，真实 Ask 链路的证据/引用 locator 携带 bbox。
//!
//! 复用生产口径：真实 catalog + 真实 Ollama 嵌入 → answer_extractively（摘录式，
//! 不调生成模型，只做检索 + 证据组装 + 引用），打印每条 citation 的 locator
//! （page_no + bbox 是否存在 + bbox 值），确认「PDF 定位到对应页并高亮」的数据
//! 链路在真实数据上成立。只读，不改生产数据。
//!
//! 用法:
//!   cargo run -p fanfan-core --example ask_bbox_probe
//! 环境变量:
//!   FANFAN_DATA_DIR      真实 catalog 目录（默认 E:\Desktop\FanFan\DATA\FanFanData）
//!   FANFAN_MODEL_STORE   模型注册表目录（默认 E:\Desktop\FanFan\DATA\FanFanModelStore）

use std::{env, path::PathBuf};

use fanfan_core::{
    AppError, AnswerStyle, AskRequest, Availability, CatalogStore, ModelManager, ModelRole,
    OllamaClient, ScopeFilter, SemanticQuery,
};

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

fn main() {
    if let Err(error) = run() {
        eprintln!("ask_bbox_probe 未完成: code={} message={}", error.code, error.message);
        if let Some(details) = error.details {
            eprintln!("  详情: {details}");
        }
        std::process::exit(1);
    }
}

fn run() -> Result<(), AppError> {
    let data_dir =
        env::var("FANFAN_DATA_DIR").unwrap_or_else(|_| r"E:\Desktop\FanFan\DATA\FanFanData".to_owned());
    let model_store = env::var("FANFAN_MODEL_STORE")
        .unwrap_or_else(|_| r"E:\Desktop\FanFan\DATA\FanFanModelStore".to_owned());

    let catalog = CatalogStore::open(PathBuf::from(&data_dir).join("fanfan.db"))?;
    let manager = ModelManager::open_store(PathBuf::from(&model_store))?;
    let embedding = manager
        .list_artifacts()?
        .into_iter()
        .find(|artifact| artifact.role == ModelRole::Embedding)
        .ok_or_else(|| AppError::new("PROBE_STATE", "注册表没有 Embedding 模型", false))?;
    if embedding.format == fanfan_core::ModelFormat::Ollama {
        manager.ollama_embedding_ready(&embedding.model_id)?;
    }
    let active = manager
        .active_artifact(ModelRole::Embedding)?
        .ok_or_else(|| AppError::new("PROBE_STATE", "没有激活的 Embedding 模型", false))?;
    let ollama = OllamaClient::local();
    let query = "2012年上半年数据库系统工程师考试下午真题 图书管理系统 顶层数据流图 外部实体有哪些";
    let prefixed = format!(
        "{}{}",
        active.query_prefix.as_deref().unwrap_or_default(),
        query
    );
    let (vectors, _dimension) = ollama.embed(&active.model_id, &[prefixed])?;
    let vector = vectors
        .into_iter()
        .next()
        .ok_or_else(|| AppError::new("PROBE_STATE", "embedding 返回空", false))?;

    let request = AskRequest {
        question: query.to_owned(),
        session_id: None,
        scope: all_authorized_scope(),
        answer_style: AnswerStyle::Detailed,
        retrieval_limit: 10,
        max_source_files: 4,
        strict_evidence: true,
        clarification_selection: None,
        clarification_message_id: None,
        think_mode: false,
    };
    let answer = catalog.answer_extractively(
        &request,
        Some(SemanticQuery {
            model_artifact_id: &active.artifact_id.to_string(),
            vector: &vector,
        }),
    )?;

    println!("== 摘录回答 ==");
    println!("{}", answer.answer);
    println!("\n== 引用 locator（page_no / bbox）==");
    let mut total_citations = 0;
    let mut with_page = 0;
    let mut with_bbox = 0;
    for claim in &answer.claims {
        for citation in &claim.citations {
            total_citations += 1;
            let page_no = citation.locator.page_no;
            let bbox = citation.locator.bbox.as_ref();
            let has_page = page_no.is_some();
            let has_bbox = bbox.is_some();
            if has_page {
                with_page += 1;
            }
            if has_bbox {
                with_bbox += 1;
            }
            println!(
                "  file={} page={:?} bbox={} quote={}",
                citation.file_id,
                page_no,
                if has_bbox { "YES" } else { "NO" },
                truncate(&citation.quote, 50)
            );
        }
    }
    println!(
        "\n== 汇总 ==\ncitations={total_citations} 带页码={with_page} 带bbox={with_bbox} files={}",
        answer.used_file_ids.len()
    );
    Ok(())
}

fn truncate(value: &str, max: usize) -> String {
    let chars: Vec<char> = value.chars().collect();
    if chars.len() <= max {
        value.to_owned()
    } else {
        let mut out: String = chars[..max].iter().collect();
        out.push('…');
        out
    }
}
