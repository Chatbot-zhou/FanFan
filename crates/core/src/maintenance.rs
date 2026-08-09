use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::AppError;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HealthCheckItem {
    pub key: String,
    pub label: String,
    pub status: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MaintenanceSnapshot {
    pub schema_version: u32,
    pub database_size_bytes: u64,
    pub indexed_files: u64,
    pub searchable_chunks: u64,
    pub embedded_chunks: u64,
    pub pending_files: u64,
    pub failed_files: u64,
    pub active_jobs: u64,
    pub log_events: u64,
    #[serde(skip_serializing)]
    pub degradation_level: String,
    #[serde(skip_serializing)]
    pub degradation_reasons: Vec<String>,
    pub background_notice: Option<String>,
    pub checks: Vec<HealthCheckItem>,
    pub checked_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MaintenanceCheckResult {
    pub level: String,
    pub database_result: String,
    pub elapsed_ms: u64,
    pub source_files_modified: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IndexActivityStats {
    pub discovered_files: u64,
    pub searchable_files: u64,
    pub parsed_files: u64,
    pub embedded_files: u64,
    pub ocr_pages: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AppLogRecord {
    pub log_id: String,
    pub level: String,
    pub component: String,
    pub event_name: String,
    pub fields: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LogQuery {
    pub cursor: Option<String>,
    pub page_size: u32,
}

impl LogQuery {
    pub fn validate(&self) -> Result<(), AppError> {
        if !(1..=1000).contains(&self.page_size) {
            return Err(AppError::new(
                "LOG_LIMIT_INVALID",
                "日志读取数量需要在1到1000之间",
                false,
            ));
        }
        self.offset().map(|_| ())
    }

    pub fn offset(&self) -> Result<u64, AppError> {
        self.cursor
            .as_deref()
            .unwrap_or("0")
            .parse::<u64>()
            .map_err(|_| AppError::new("LOG_CURSOR_INVALID", "日志分页游标无效", false))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LogPage {
    pub items: Vec<AppLogRecord>,
    pub next_cursor: Option<String>,
    pub total: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IndexRebuildResult {
    pub reset_files: u64,
    pub removed_nodes: u64,
    pub removed_chunks: u64,
    pub removed_embeddings: u64,
    pub source_files_modified: bool,
}

pub fn validate_rebuild_confirmation(value: &str) -> Result<(), AppError> {
    if value != "REBUILD_INDEX" {
        return Err(AppError::new(
            "INDEX_REBUILD_CONFIRMATION_REQUIRED",
            "重建索引需要明确确认",
            false,
        ));
    }
    Ok(())
}
