//! 基准：ModelManager::open_store 耗时（验证快速路径跳过全量 SHA-256）。
//! 用法: cargo run --example open_store_bench -- <model_store_root>

use std::path::PathBuf;
use std::time::Instant;

use fanfan_core::ModelManager;

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
}
