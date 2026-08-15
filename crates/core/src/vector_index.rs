use std::{
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
    time::SystemTime,
};

use usearch::{Index, IndexOptions, MetricKind, ScalarKind};

use crate::AppError;

#[derive(Debug, Clone, PartialEq)]
pub struct VectorMatch {
    pub key: u64,
    pub distance: f32,
}

/// 已打开的视图索引缓存：restore_view 每次都会重建 HNSW 图结构（实测约 6 秒），
/// 查询路径复用同一实例可省掉该固定开销。索引重建会替换文件（mtime 变化），
/// 因此 build 前必须调用 `clear_index_cache` 释放旧句柄，否则 Windows 上
/// 文件删除/替换会因占用而失败。
struct CachedIndex {
    path: PathBuf,
    modified: SystemTime,
    index: Index,
}

static INDEX_CACHE: Mutex<Option<CachedIndex>> = Mutex::new(None);

pub fn clear_index_cache() {
    *INDEX_CACHE.lock().expect("index cache poisoned") = None;
}

pub fn build_index(
    target_path: &Path,
    dimension: usize,
    entries: &[(u64, Vec<f32>)],
) -> Result<(), AppError> {
    let borrowed = entries
        .iter()
        .map(|(key, vector)| (*key, vector.as_slice()))
        .collect::<Vec<_>>();
    build_index_refs(target_path, dimension, &borrowed)
}

pub fn build_index_refs(
    target_path: &Path,
    dimension: usize,
    entries: &[(u64, &[f32])],
) -> Result<(), AppError> {
    // 释放旧句柄，避免 Windows 上文件替换因占用而失败。
    clear_index_cache();
    if dimension == 0
        || entries.iter().any(|(key, vector)| {
            *key == 0 || vector.len() != dimension || vector.iter().any(|value| !value.is_finite())
        })
    {
        return Err(vector_error(
            "VECTOR_INDEX_INPUT_INVALID",
            "向量索引输入的键、维度或数值无效",
            false,
        ));
    }
    if let Some(parent) = target_path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            vector_error("VECTOR_INDEX_DIRECTORY_FAILED", error.to_string(), true)
        })?;
    }
    let temporary_path = target_path.with_extension("usearch.new");
    if temporary_path.exists() {
        fs::remove_file(&temporary_path).map_err(|error| {
            vector_error("VECTOR_INDEX_TEMP_CLEANUP_FAILED", error.to_string(), true)
        })?;
    }
    let options = IndexOptions {
        dimensions: dimension,
        metric: MetricKind::Cos,
        quantization: ScalarKind::BF16,
        ..Default::default()
    };
    let index = Index::new(&options)
        .map_err(|error| vector_error("VECTOR_INDEX_CREATE_FAILED", error.to_string(), false))?;
    // 限制构建线程数：usearch 默认用满所有核，19.9 万条全量重建会打满 CPU，
    // 饿死桌面端全部页面查询（观察：构建期间命令排队 10-38s）。2 线程足够
    // 快速完成，同时把大部分核留给交互。
    index
        .reserve_capacity_and_threads(entries.len(), 2)
        .map_err(|error| vector_error("VECTOR_INDEX_RESERVE_FAILED", error.to_string(), true))?;
    for (key, vector) in entries {
        index
            .add(*key, vector)
            .map_err(|error| vector_error("VECTOR_INDEX_ADD_FAILED", error.to_string(), true))?;
    }
    let temporary_text = path_text(&temporary_path)?;
    index
        .save(temporary_text)
        .map_err(|error| vector_error("VECTOR_INDEX_SAVE_FAILED", error.to_string(), true))?;
    validate_index(&temporary_path, dimension, entries.len())?;
    if target_path.exists() {
        fs::remove_file(target_path).map_err(|error| {
            vector_error("VECTOR_INDEX_REPLACE_FAILED", error.to_string(), true)
        })?;
    }
    fs::rename(&temporary_path, target_path)
        .map_err(|error| vector_error("VECTOR_INDEX_REPLACE_FAILED", error.to_string(), true))
}

pub fn search_index(
    index_path: &Path,
    query: &[f32],
    count: usize,
) -> Result<Vec<VectorMatch>, AppError> {
    if query.is_empty() || query.iter().any(|value| !value.is_finite()) || count == 0 {
        return Err(vector_error(
            "VECTOR_INDEX_QUERY_INVALID",
            "向量索引查询无效",
            false,
        ));
    }
    let modified = fs::metadata(index_path)
        .and_then(|meta| meta.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let mut cache = INDEX_CACHE.lock().expect("index cache poisoned");
    let index = match cache.as_ref() {
        Some(entry) if entry.path == index_path && entry.modified == modified => &entry.index,
        _ => {
            let restored = Index::restore_view(path_text(index_path)?)
                .map_err(|error| vector_error("VECTOR_INDEX_OPEN_FAILED", error.to_string(), true))?;
            *cache = Some(CachedIndex {
                path: index_path.to_path_buf(),
                modified,
                index: restored,
            });
            &cache.as_ref().expect("just inserted").index
        }
    };
    if index.dimensions() != query.len() {
        return Err(vector_error(
            "VECTOR_INDEX_DIMENSION_MISMATCH",
            format!(
                "索引维度{}与查询维度{}不一致",
                index.dimensions(),
                query.len()
            ),
            false,
        ));
    }
    let matches = index
        .search(query, count.min(index.size()))
        .map_err(|error| vector_error("VECTOR_INDEX_SEARCH_FAILED", error.to_string(), true))?;
    Ok(matches
        .keys
        .into_iter()
        .zip(matches.distances)
        .map(|(key, distance)| VectorMatch { key, distance })
        .collect())
}

pub fn validate_index(
    index_path: &Path,
    expected_dimension: usize,
    expected_size: usize,
) -> Result<(), AppError> {
    let index = Index::restore_view(path_text(index_path)?)
        .map_err(|error| vector_error("VECTOR_INDEX_OPEN_FAILED", error.to_string(), true))?;
    if index.dimensions() != expected_dimension || index.size() != expected_size {
        return Err(vector_error(
            "VECTOR_INDEX_SELF_TEST_FAILED",
            format!(
                "索引自检失败：维度{}/{}，向量数{}/{}",
                index.dimensions(),
                expected_dimension,
                index.size(),
                expected_size
            ),
            false,
        ));
    }
    Ok(())
}

fn path_text(path: &Path) -> Result<&str, AppError> {
    path.to_str().ok_or_else(|| {
        vector_error(
            "VECTOR_INDEX_PATH_INVALID",
            "向量索引路径无法转换为本地Unicode路径",
            false,
        )
    })
}

fn vector_error(code: &str, message: impl Into<String>, retryable: bool) -> AppError {
    AppError::new(code, message.into(), retryable)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_index_round_trip_returns_nearest_vector() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("test.usearch");
        build_index(
            &path,
            3,
            &[
                (1, vec![1.0, 0.0, 0.0]),
                (2, vec![0.0, 1.0, 0.0]),
                (3, vec![0.0, 0.0, 1.0]),
            ],
        )
        .expect("build");
        let matches = search_index(&path, &[0.95, 0.05, 0.0], 2).expect("search");
        assert_eq!(matches.first().map(|item| item.key), Some(1));
    }
}
