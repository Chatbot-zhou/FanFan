use std::{env, path::PathBuf};

use rusqlite::{Connection, OpenFlags};
use serde_json::json;

fn main() {
    if let Err(error) = run() {
        eprintln!("本地处理状态检查失败: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let data_directory = env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join("com.fanfan.desktop");
    let database = data_directory.join("fanfan.db");
    let connection = Connection::open_with_flags(
        format!("file:{}?mode=ro", database.to_string_lossy()),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )?;
    connection.busy_timeout(std::time::Duration::from_millis(500))?;

    let parse_states = grouped_counts(&connection, "parse_status", "files")?;
    let dispositions = grouped_counts(&connection, "processing_disposition", "files")?;
    let parse_attempts = connection.query_row(
        "SELECT COUNT(*) FROM processing_attempts WHERE operation = 'parse'",
        [],
        |row| row.get::<_, u64>(0),
    )?;
    let privacy = fanfan_core::inspect_runtime_log_privacy(&data_directory.join("logs"))?;
    let output = json!({
        "files": scalar(&connection, "SELECT COUNT(*) FROM files")?,
        "roots": scalar(&connection, "SELECT COUNT(*) FROM roots WHERE enabled = 1")?,
        "chunks": scalar(&connection, "SELECT COUNT(*) FROM chunks")?,
        "embeddings": scalar(&connection, "SELECT COUNT(*) FROM chunk_embeddings")?,
        "active_vector_keys": scalar(&connection, "SELECT COUNT(*) FROM vector_index_keys")?,
        "image_assets": scalar(&connection, "SELECT COUNT(*) FROM image_assets")?,
        "parse_states": parse_states,
        "dispositions": dispositions,
        "parse_attempts": parse_attempts,
        "stale_active_jobs": scalar(
            &connection,
            "SELECT COUNT(*) FROM jobs WHERE status IN ('queued','running') AND julianday(COALESCE(started_at, created_at)) < julianday('now', '-15 minutes')",
        )?,
        "privacy": {
            "log_files_checked": privacy.files_checked,
            "events_checked": privacy.events_checked,
            "violations": privacy.violations,
        },
    });
    println!("{}", serde_json::to_string(&output)?);
    Ok(())
}

fn scalar(connection: &Connection, query: &str) -> rusqlite::Result<u64> {
    connection.query_row(query, [], |row| row.get(0))
}

fn grouped_counts(
    connection: &Connection,
    column: &str,
    table: &str,
) -> Result<serde_json::Map<String, serde_json::Value>, Box<dyn std::error::Error>> {
    let allowed = matches!(
        (table, column),
        ("files", "parse_status") | ("files", "processing_disposition")
    );
    if !allowed {
        return Err("unsupported aggregate".into());
    }
    let mut statement = connection.prepare(&format!(
        "SELECT {column}, COUNT(*) FROM {table} GROUP BY {column} ORDER BY {column}"
    ))?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
    })?;
    let mut output = serde_json::Map::new();
    for row in rows {
        let (key, value) = row?;
        output.insert(key, value.into());
    }
    Ok(output)
}
