//! 基准：ModelManager::open_store 耗时（验证快速路径跳过全量 SHA-256）。
//! 用法: cargo run --example open_store_bench -- <model_store_root> [legacy_root]

use std::path::PathBuf;
use std::time::Instant;

use fanfan_core::ModelManager;
use fanfan_core::locked_download_artifact;

fn main() {
    let root = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "C:/Users/23255/AppData/Local/FanFan/ModelStore".into());
    let root = PathBuf::from(root);
    let started = Instant::now();
    let manager = ModelManager::open_store(root).expect("open_store");
    let elapsed = started.elapsed();
    println!("open_store total: {:?}", elapsed);
    println!(
        "status: {:?}",
        manager.store_status().map(|s| s.integrity_status)
    );

    // 逐 artifact 分解 restore 内部逻辑：locked 解析耗时 + 每个 companion
    // 目标文件是否存在/大小是否匹配（不哈希）。
    let registry = manager.registry_state().expect("registry_state");
    for artifact in &registry.artifacts {
        let resolved = Instant::now();
        let Some(locked) = locked_download_artifact(&artifact.model_id, artifact.source) else {
            println!(
                "[{}] 无 locked 定义（跳过），resolve={:?}",
                artifact.model_id,
                resolved.elapsed()
            );
            continue;
        };
        let parent = PathBuf::from(&artifact.local_path)
            .parent()
            .map(PathBuf::from)
            .expect("parent");
        let mut problems = Vec::new();
        for expected in &locked.companion_files {
            let target = parent.join(&expected.file_name);
            match std::fs::metadata(&target) {
                Ok(meta) if meta.is_file() => {
                    if meta.len() != expected.size_bytes {
                        problems.push(format!(
                            "{}: size {} != 期望 {}",
                            expected.file_name,
                            meta.len(),
                            expected.size_bytes
                        ));
                    }
                }
                Ok(_) => problems.push(format!("{}: 存在但不是文件", expected.file_name)),
                Err(e) => problems.push(format!("{}: 缺失 ({e})", expected.file_name)),
            }
        }
        println!(
            "[{}] 主文件={} source={:?} 组件数={} resolve={:?} dir={}",
            artifact.model_id,
            artifact.size_bytes,
            artifact.source,
            locked.companion_files.len(),
            resolved.elapsed(),
            parent.display()
        );
        for p in &problems {
            println!("    ✗ {p}");
        }
        if problems.is_empty() {
            println!("    ✓ 全部匹配");
        }
    }

    let legacy = std::env::args()
        .nth(2)
        .map(PathBuf::from)
        .into_iter()
        .collect::<Vec<_>>();
    if !legacy.is_empty() {
        let started = Instant::now();
        let restored = manager
            .restore_locked_companions_from(&legacy)
            .expect("restore companions");
        println!(
            "restore_locked_companions_from: {:?} restored={restored}",
            started.elapsed()
        );
    }
}
