//! 探针：用与桌面应用完全一致的 WorkerClient 路径（`\\?\` 前缀可执行文件路径）
//! 启动 speech/onnx 角色 worker 并发送请求。
//!
//! 判定方法：supports() 在任何模型加载之前执行——若角色未生效，请求会立刻
//! 返回 OPERATION_UNSUPPORTED；角色生效则返回模型路径类错误（不同错误码）。
//! 用法: cargo run --example worker_probe

use std::path::PathBuf;

use fanfan_core::{EmbeddingRequest, WorkerClient, WorkerRole};

fn main() {
    let prefixed = r"\\?\E:\Desktop\FanFan\target\debug\worker\fanfan-worker.exe";
    let clean = r"E:\Desktop\FanFan\target\debug\worker\fanfan-worker.exe";
    for (label, path) in [("clean", clean), ("prefixed", prefixed)] {
        // 与 apps/desktop/src-tauri/src/lib.rs 完全一致的写法（clone + with_role）
        let asr = WorkerClient::from_executable(PathBuf::from(path))
            .clone()
            .with_role(WorkerRole::Speech);
        match asr.self_test_asr(
            r"C:\no\such\model.onnx".into(),
            r"C:\no\such\tokens.txt".into(),
            2,
            "sense_voice".to_owned(),
        ) {
            Ok(result) => println!("[{label} speech.asr_self_test] OK {:?}", result),
            Err(error) => println!(
                "[{label} speech.asr_self_test] code={} (OPERATION_UNSUPPORTED=角色未生效) msg={}",
                error.code, error.message
            ),
        }
        let onnx = WorkerClient::from_executable(PathBuf::from(path))
            .clone()
            .with_role(WorkerRole::Onnx);
        match onnx.encode_embeddings(&EmbeddingRequest {
            model_path: r"C:\no\such\model.onnx".into(),
            tokenizer_path: Some(r"C:\no\such\tokenizer.json".into()),
            texts: vec!["测试".into()],
            max_length: 512,
            threads: 2,
        }) {
            Ok(result) => println!("[{label} embedding.encode] OK {:?}", result),
            Err(error) => println!(
                "[{label} embedding.encode] code={} (OPERATION_UNSUPPORTED=角色未生效) msg={}",
                error.code, error.message
            ),
        }
    }
}
