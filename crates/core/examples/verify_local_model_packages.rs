use std::{env, path::PathBuf};

use fanfan_core::{AppError, ModelManager, ModelRole, WorkerClient};

fn main() {
    if let Err(error) = run() {
        eprintln!("模型包自检未完成: code={}", error.code);
        if let Some(details) = error.details {
            eprintln!("technical={details}");
        }
        std::process::exit(1);
    }
}

fn run() -> Result<(), AppError> {
    let arguments = env::args().skip(1).collect::<Vec<_>>();
    let use_packaged_worker = arguments
        .iter()
        .any(|argument| argument == "--packaged-worker");
    let verify_all = arguments.iter().any(|argument| argument == "--verify-all");
    let repository_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let model_store = env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join("FanFan/ModelStore/v1");
    let manager = ModelManager::open_store(model_store)?;
    let worker_python = repository_root.join(".artifacts/packaging-venv/Scripts/python.exe");
    let packaged_worker = repository_root.join("target/debug/worker/fanfan-worker.exe");
    let worker = if use_packaged_worker && packaged_worker.is_file() {
        WorkerClient::from_executable(packaged_worker)
    } else if worker_python.is_file() {
        WorkerClient::new(
            worker_python.into_os_string(),
            repository_root.join("services/worker"),
        )
    } else if packaged_worker.is_file() {
        WorkerClient::from_executable(packaged_worker)
    } else {
        WorkerClient::from_environment(repository_root.join("services/worker"))
    };
    let registry = manager.registry_state()?;
    let mut activated = Vec::new();
    let mut failures = Vec::new();
    for artifact in registry.artifacts.into_iter().filter(|artifact| {
        artifact.status == "pending_self_test"
            || (verify_all && matches!(artifact.status.as_str(), "ready" | "active"))
    }) {
        let parent = PathBuf::from(&artifact.local_path)
            .parent()
            .map(PathBuf::from)
            .ok_or_else(|| {
                AppError::new(
                    "MODEL_PACKAGE_PATH_INVALID",
                    "模型包没有有效的本地目录",
                    false,
                )
            })?;
        let result = match artifact.role {
            ModelRole::Ocr => worker
                .self_test_ocr(
                    artifact.local_path.clone(),
                    required_companion(&parent, "ch_PP-OCRv5_mobile_det.onnx")?,
                    required_companion(&parent, "ch_ppocr_mobile_v2.0_cls_infer.onnx")?,
                    required_companion(&parent, "ppocrv5_dict.txt")?,
                    1,
                    "PPOCRV5".to_owned(),
                )
                .and_then(|_| manager.activate_artifact(&artifact.artifact_id, None))
                .map(|_| "ocr"),
            ModelRole::Asr => worker
                .self_test_asr(
                    artifact.local_path.clone(),
                    required_companion(&parent, "tokens.txt")?,
                    1,
                    "paraformer".to_owned(),
                )
                .and_then(|_| manager.activate_artifact(&artifact.artifact_id, None))
                .map(|_| "asr"),
            _ => continue,
        };
        match result {
            Ok(role) => activated.push(role),
            Err(error) => failures.push(format!(
                "{}:{}:{}",
                artifact.role.role_name(),
                error.code,
                safe_diagnostic(&error.message)
            )),
        }
    }
    println!(
        "模型包自检完成: activated_roles={}, failures={}",
        if activated.is_empty() {
            "none".to_owned()
        } else {
            activated.join(",")
        },
        if failures.is_empty() {
            "none".to_owned()
        } else {
            failures.join(",")
        },
    );
    Ok(())
}

trait ModelRoleLabel {
    fn role_name(self) -> &'static str;
}

impl ModelRoleLabel for ModelRole {
    fn role_name(self) -> &'static str {
        match self {
            ModelRole::Generation => "generation",
            ModelRole::Embedding => "embedding",
            ModelRole::Vision => "vision",
            ModelRole::Reranker => "reranker",
            ModelRole::Ocr => "ocr",
            ModelRole::Asr => "asr",
            ModelRole::Router => "router",
        }
    }
}

fn required_companion(parent: &std::path::Path, name: &str) -> Result<String, AppError> {
    let path = parent.join(name);
    if !path.is_file() {
        return Err(AppError::new(
            "MODEL_PACKAGE_INCOMPLETE",
            format!("模型包缺少配套文件：{name}"),
            false,
        ));
    }
    Ok(path.to_string_lossy().into_owned())
}

fn safe_diagnostic(message: &str) -> String {
    message
        .split_whitespace()
        .take(18)
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(240)
        .collect()
}
