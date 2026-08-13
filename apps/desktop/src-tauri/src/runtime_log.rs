use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};

use chrono::{DateTime, Utc};
use fanfan_core::{AppError, AppLogRecord, LogPage, LogQuery, sanitize_log_value};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

const LOG_FILE_NAME: &str = "runtime-current.jsonl";
const SESSION_MARKER_NAME: &str = "active-session.json";
const MAX_LOG_FILE_BYTES: u64 = 5 * 1024 * 1024;
const ROTATED_LOG_FILES: usize = 4;
const MAX_EVENT_BYTES: usize = 32 * 1024;

static RUNTIME_LOGGER: OnceLock<RuntimeLogger> = OnceLock::new();

#[derive(Debug, Clone, Serialize)]
pub struct RuntimeLogInitialization {
    pub session_id: String,
    pub previous_session_unclean: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct RuntimeLogEntry {
    schema_version: u32,
    event_id: String,
    session_id: String,
    sequence: u64,
    created_at: String,
    level: String,
    component: String,
    event_name: String,
    correlation_id: Option<String>,
    fields: Value,
}

#[derive(Debug)]
struct RuntimeLogger {
    directory: PathBuf,
    session_id: String,
    sequence: AtomicU64,
    entry_count: AtomicU64,
    write_lock: Mutex<()>,
}

impl RuntimeLogger {
    fn new(directory: PathBuf) -> Result<(Self, bool), AppError> {
        fs::create_dir_all(&directory).map_err(|error| {
            AppError::new("RUNTIME_LOG_DIRECTORY_FAILED", error.to_string(), true)
        })?;
        let marker_path = directory.join(SESSION_MARKER_NAME);
        let previous_session_unclean = marker_path.is_file();
        let entry_count = count_existing_entries(&directory);
        let session_id = Uuid::now_v7().to_string();
        let marker = serde_json::to_vec_pretty(&json!({
            "session_id": session_id,
            "started_at": Utc::now().to_rfc3339(),
            "app_version": env!("CARGO_PKG_VERSION"),
        }))
        .map_err(|error| AppError::new("RUNTIME_LOG_SERIALIZE_FAILED", error.to_string(), false))?;
        write_replaced_file(&marker_path, &marker)?;
        Ok((
            Self {
                directory,
                session_id,
                sequence: AtomicU64::new(0),
                entry_count: AtomicU64::new(entry_count),
                write_lock: Mutex::new(()),
            },
            previous_session_unclean,
        ))
    }

    fn record(
        &self,
        level: &str,
        component: &str,
        event_name: &str,
        correlation_id: Option<&str>,
        fields: &Value,
    ) -> Result<(), AppError> {
        let safe_fields = sanitize_log_value(fields, None);
        let mut entry = RuntimeLogEntry {
            schema_version: 1,
            event_id: Uuid::now_v7().to_string(),
            session_id: self.session_id.clone(),
            sequence: self.sequence.fetch_add(1, Ordering::Relaxed) + 1,
            created_at: Utc::now().to_rfc3339(),
            level: normalize_level(level).to_owned(),
            component: component.to_owned(),
            event_name: event_name.to_owned(),
            correlation_id: correlation_id.map(str::to_owned),
            fields: safe_fields,
        };
        let mut serialized = serde_json::to_vec(&entry).map_err(|error| {
            AppError::new("RUNTIME_LOG_SERIALIZE_FAILED", error.to_string(), false)
        })?;
        if serialized.len() > MAX_EVENT_BYTES {
            entry.fields = json!({
                "truncated": true,
                "original_size_bytes": serialized.len(),
            });
            serialized = serde_json::to_vec(&entry).map_err(|error| {
                AppError::new("RUNTIME_LOG_SERIALIZE_FAILED", error.to_string(), false)
            })?;
        }
        serialized.push(b'\n');

        let _guard = self.write_lock.lock().map_err(|_| {
            AppError::new("RUNTIME_LOG_LOCK_FAILED", "运行日志写入锁已经损坏", true)
        })?;
        self.rotate_if_needed(serialized.len() as u64)?;
        let path = self.directory.join(LOG_FILE_NAME);
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|error| AppError::new("RUNTIME_LOG_WRITE_FAILED", error.to_string(), true))?;
        file.write_all(&serialized)
            .and_then(|_| file.flush())
            .map_err(|error| AppError::new("RUNTIME_LOG_WRITE_FAILED", error.to_string(), true))?;
        self.entry_count.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn rotate_if_needed(&self, incoming_bytes: u64) -> Result<(), AppError> {
        let current = self.directory.join(LOG_FILE_NAME);
        let current_bytes = fs::metadata(&current).map(|item| item.len()).unwrap_or(0);
        if current_bytes.saturating_add(incoming_bytes) <= MAX_LOG_FILE_BYTES {
            return Ok(());
        }
        let oldest = self.rotated_path(ROTATED_LOG_FILES);
        if oldest.exists() {
            let removed = count_file_entries(&oldest);
            fs::remove_file(&oldest).map_err(|error| {
                AppError::new("RUNTIME_LOG_ROTATE_FAILED", error.to_string(), true)
            })?;
            let _ =
                self.entry_count
                    .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                        Some(current.saturating_sub(removed))
                    });
        }
        for index in (1..ROTATED_LOG_FILES).rev() {
            let source = self.rotated_path(index);
            if source.exists() {
                fs::rename(&source, self.rotated_path(index + 1)).map_err(|error| {
                    AppError::new("RUNTIME_LOG_ROTATE_FAILED", error.to_string(), true)
                })?;
            }
        }
        if current.exists() {
            fs::rename(current, self.rotated_path(1)).map_err(|error| {
                AppError::new("RUNTIME_LOG_ROTATE_FAILED", error.to_string(), true)
            })?;
        }
        Ok(())
    }

    fn rotated_path(&self, index: usize) -> PathBuf {
        self.directory.join(format!("runtime-{index}.jsonl"))
    }

    #[cfg(test)]
    fn entries_newest_first(&self) -> Result<Vec<RuntimeLogEntry>, AppError> {
        self.entries_page(0, usize::MAX)
    }

    fn entries_page(
        &self,
        mut skip: usize,
        limit: usize,
    ) -> Result<Vec<RuntimeLogEntry>, AppError> {
        let _guard = self.write_lock.lock().map_err(|_| {
            AppError::new("RUNTIME_LOG_LOCK_FAILED", "运行日志读取锁已经损坏", true)
        })?;
        let mut paths = vec![self.directory.join(LOG_FILE_NAME)];
        paths.extend((1..=ROTATED_LOG_FILES).map(|index| self.rotated_path(index)));
        let mut entries = Vec::new();
        for path in paths {
            let contents = match fs::read_to_string(&path) {
                Ok(contents) => contents,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(AppError::new(
                        "RUNTIME_LOG_READ_FAILED",
                        error.to_string(),
                        true,
                    ));
                }
            };
            for entry in contents
                .lines()
                .rev()
                .filter_map(|line| serde_json::from_str::<RuntimeLogEntry>(line).ok())
            {
                if skip > 0 {
                    skip -= 1;
                    continue;
                }
                entries.push(entry);
                if entries.len() >= limit {
                    return Ok(entries);
                }
            }
        }
        Ok(entries)
    }

    fn clear(&self) -> Result<u64, AppError> {
        let _guard = self.write_lock.lock().map_err(|_| {
            AppError::new("RUNTIME_LOG_LOCK_FAILED", "运行日志清理锁已经损坏", true)
        })?;
        for path in std::iter::once(self.directory.join(LOG_FILE_NAME))
            .chain((1..=ROTATED_LOG_FILES).map(|index| self.rotated_path(index)))
        {
            if !path.exists() {
                continue;
            }
            fs::remove_file(path).map_err(|error| {
                AppError::new("RUNTIME_LOG_CLEAR_FAILED", error.to_string(), true)
            })?;
        }
        Ok(self.entry_count.swap(0, Ordering::Relaxed))
    }

    fn mark_clean_shutdown(&self) -> Result<(), AppError> {
        let marker = self.directory.join(SESSION_MARKER_NAME);
        match fs::remove_file(marker) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(AppError::new(
                "RUNTIME_LOG_SESSION_MARKER_FAILED",
                error.to_string(),
                true,
            )),
        }
    }
}

pub fn initialize(directory: PathBuf) -> Result<RuntimeLogInitialization, AppError> {
    if let Some(logger) = RUNTIME_LOGGER.get() {
        return Ok(RuntimeLogInitialization {
            session_id: logger.session_id.clone(),
            previous_session_unclean: false,
        });
    }
    let (logger, previous_session_unclean) = RuntimeLogger::new(directory)?;
    let session_id = logger.session_id.clone();
    RUNTIME_LOGGER.set(logger).map_err(|_| {
        AppError::new(
            "RUNTIME_LOG_ALREADY_INITIALIZED",
            "运行日志已经初始化",
            false,
        )
    })?;
    event(
        "info",
        "application",
        "session.started",
        None,
        &json!({
            "app_version": env!("CARGO_PKG_VERSION"),
            "previous_session_unclean": previous_session_unclean,
        }),
    );
    if previous_session_unclean {
        event(
            "warning",
            "application",
            "session.previous_unclean_exit",
            None,
            &json!({ "recovery_action": "review_previous_runtime_logs" }),
        );
    }
    Ok(RuntimeLogInitialization {
        session_id,
        previous_session_unclean,
    })
}

pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| info.payload().downcast_ref::<String>().map(String::as_str))
            .unwrap_or("non-string panic payload");
        let location = info.location();
        event(
            "error",
            "application",
            "runtime.panic",
            None,
            &json!({
                "message": payload,
                "source_path": location.map(|value| value.file()),
                "line": location.map(|value| value.line()),
                "column": location.map(|value| value.column()),
                "thread": std::thread::current().name().unwrap_or("unnamed"),
            }),
        );
        previous(info);
    }));
}

pub fn event(
    level: &str,
    component: &str,
    event_name: &str,
    correlation_id: Option<&str>,
    fields: &Value,
) {
    if let Some(logger) = RUNTIME_LOGGER.get() {
        let _ = logger.record(level, component, event_name, correlation_id, fields);
    }
}

pub fn query(request: &LogQuery) -> Result<LogPage, AppError> {
    request.validate()?;
    let Some(logger) = RUNTIME_LOGGER.get() else {
        return Ok(LogPage {
            items: Vec::new(),
            next_cursor: None,
            total: 0,
        });
    };
    let offset = request.offset()? as usize;
    let total = logger.entry_count.load(Ordering::Relaxed) as usize;
    let entries = logger.entries_page(offset, request.page_size as usize)?;
    let items = entries
        .into_iter()
        .filter_map(runtime_entry_to_app_log)
        .collect::<Vec<_>>();
    let consumed = offset.saturating_add(items.len());
    Ok(LogPage {
        items,
        next_cursor: (consumed < total).then(|| consumed.to_string()),
        total: total as u64,
    })
}

pub fn recent_values(limit: usize) -> Result<Vec<Value>, AppError> {
    let Some(logger) = RUNTIME_LOGGER.get() else {
        return Ok(Vec::new());
    };
    logger
        .entries_page(0, limit)?
        .into_iter()
        .take(limit)
        .map(|entry| {
            serde_json::to_value(entry).map_err(|error| {
                AppError::new("RUNTIME_LOG_SERIALIZE_FAILED", error.to_string(), false)
            })
        })
        .collect()
}

pub fn count() -> Result<u64, AppError> {
    let Some(logger) = RUNTIME_LOGGER.get() else {
        return Ok(0);
    };
    Ok(logger.entry_count.load(Ordering::Relaxed))
}

pub fn clear() -> Result<u64, AppError> {
    let Some(logger) = RUNTIME_LOGGER.get() else {
        return Ok(0);
    };
    logger.clear()
}

pub fn mark_clean_shutdown() {
    event(
        "info",
        "application",
        "session.clean_shutdown",
        None,
        &json!({}),
    );
    if let Some(logger) = RUNTIME_LOGGER.get() {
        let _ = logger.mark_clean_shutdown();
    }
}

pub fn session_id() -> Option<String> {
    RUNTIME_LOGGER.get().map(|logger| logger.session_id.clone())
}

fn runtime_entry_to_app_log(mut entry: RuntimeLogEntry) -> Option<AppLogRecord> {
    let mut fields = entry.fields.as_object().cloned().unwrap_or_default();
    fields.insert("session_id".to_owned(), Value::String(entry.session_id));
    fields.insert("sequence".to_owned(), Value::from(entry.sequence));
    if let Some(correlation_id) = entry.correlation_id.take() {
        fields.insert("correlation_id".to_owned(), Value::String(correlation_id));
    }
    Some(AppLogRecord {
        log_id: entry.event_id,
        level: entry.level,
        component: entry.component,
        event_name: entry.event_name,
        fields: Value::Object(fields),
        created_at: DateTime::parse_from_rfc3339(&entry.created_at)
            .ok()?
            .with_timezone(&Utc),
    })
}

fn normalize_level(level: &str) -> &'static str {
    match level {
        "error" => "error",
        "warning" | "warn" => "warning",
        "debug" => "debug",
        _ => "info",
    }
}

fn count_existing_entries(directory: &Path) -> u64 {
    std::iter::once(directory.join(LOG_FILE_NAME))
        .chain(
            (1..=ROTATED_LOG_FILES).map(|index| directory.join(format!("runtime-{index}.jsonl"))),
        )
        .map(|path| count_file_entries(&path))
        .sum()
}

fn count_file_entries(path: &Path) -> u64 {
    fs::read(path)
        .map(|bytes| bytes.iter().filter(|byte| **byte == b'\n').count() as u64)
        .unwrap_or(0)
}

fn write_replaced_file(path: &Path, bytes: &[u8]) -> Result<(), AppError> {
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, bytes).map_err(|error| {
        AppError::new("RUNTIME_LOG_SESSION_MARKER_FAILED", error.to_string(), true)
    })?;
    if path.exists() {
        fs::remove_file(path).map_err(|error| {
            AppError::new("RUNTIME_LOG_SESSION_MARKER_FAILED", error.to_string(), true)
        })?;
    }
    fs::rename(temporary, path).map_err(|error| {
        AppError::new("RUNTIME_LOG_SESSION_MARKER_FAILED", error.to_string(), true)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_logger_persists_sanitized_correlated_events() {
        let directory = tempfile::tempdir().expect("tempdir");
        let (logger, previous_unclean) =
            RuntimeLogger::new(directory.path().to_path_buf()).expect("logger");
        assert!(!previous_unclean);
        logger
            .record(
                "error",
                "frontend",
                "action.failed",
                Some("corr-1"),
                &json!({
                    "question": "我的隐私问题",
                    "message": r#"打开 C:\\Users\\someone\\secret.docx 失败"#,
                    "error_code": "OPEN_FAILED",
                }),
            )
            .expect("record");

        let entries = logger.entries_newest_first().expect("entries");
        assert_eq!(entries.len(), 1);
        assert_eq!(logger.entry_count.load(Ordering::Relaxed), 1);
        assert_eq!(logger.entries_page(0, 1).expect("page").len(), 1);
        assert_eq!(entries[0].correlation_id.as_deref(), Some("corr-1"));
        assert_eq!(entries[0].fields["question"], "[redacted]");
        assert!(
            !entries[0].fields["message"]
                .as_str()
                .unwrap()
                .contains("Users")
        );
        assert_eq!(entries[0].fields["error_code"], "OPEN_FAILED");
        assert_eq!(logger.clear().expect("clear"), 1);
        assert_eq!(logger.entry_count.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn an_existing_session_marker_is_reported_as_unclean_exit() {
        let directory = tempfile::tempdir().expect("tempdir");
        fs::write(directory.path().join(SESSION_MARKER_NAME), b"previous")
            .expect("previous marker");
        let (logger, previous_unclean) =
            RuntimeLogger::new(directory.path().to_path_buf()).expect("logger");
        assert!(previous_unclean);
        logger.mark_clean_shutdown().expect("clean marker");
        assert!(!directory.path().join(SESSION_MARKER_NAME).exists());
    }
}
