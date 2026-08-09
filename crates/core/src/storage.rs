use std::collections::{BTreeMap, HashSet};
use std::{fs, fs::File, io::Read, path::PathBuf, time::Duration};

use chrono::{DateTime, Utc};
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OptionalExtension, Transaction, params, params_from_iter};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    AnswerResult, AnswerSourceFile, AppError, AppLogRecord, AskRequest, AuthorizationSource,
    BUILT_IN_EXCLUSION_RULES, CandidateRoot, CandidateRootStatus, CandidateRootType,
    CheckpointStatus, CheckpointType, ChunkEmbeddingInput, CollectionKind, CollectionRecord,
    CollectionRule, CreateCollectionRequest, DegradationLevel, DegradationState, DiscoveredFile,
    DocumentNode, ExclusionRule, ExclusionRuleClass, ExclusionRuleType, ExplorationCandidate,
    ExtractionChunk, ExtractionDocument, ExtractionRunRequest, ExtractionRunResult,
    ExtractionTable, FilePage, FileQuery, FileRecord, FileRelation, FileSystemEvent,
    HealthCheckItem, InboxEventType, InboxItem, InboxPage, InboxQuery, InboxUpdateRequest,
    IndexActivityStats, IndexRebuildResult, JobRecord, JobStatus, LogPage, LogQuery,
    MaintenanceSnapshot, ParseOutcome, ParseResult, ParseStatus, PendingEmbeddingChunk, RankedHit,
    RelationPage, RelationQuery, RelationRefreshResult, RelationType, RootKind, RootRecord,
    RootSource, RootStatus, ScanOutcome, ScopeFilter, SearchMode, SemanticQuery, SourceLocator,
    TaskPlan, TaskStep, TriageStatus, ValidationCheckpoint, VolumeType, WatchMode,
    chunks_from_nodes, fts_query, normalized_version_key,
};

pub const CURRENT_SCHEMA_VERSION: u32 = 6;

struct Migration {
    version: u32,
    name: &'static str,
    sql: &'static str,
}

const ROOT_SELECT: &str = "SELECT root_id, path, canonical_path, path_key, root_file_id, volume_id, volume_type, authorization_source, root_kind, label, enabled, status, watch_mode, coverage_parent_root_id, file_count, permission_error_count, last_scan_at FROM roots";

const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "catalog_foundation",
        sql: r#"
            CREATE TABLE IF NOT EXISTS roots (
                root_id TEXT PRIMARY KEY,
                label TEXT NOT NULL,
                canonical_path TEXT NOT NULL,
                path_key TEXT NOT NULL UNIQUE,
                source TEXT NOT NULL,
                status TEXT NOT NULL,
                file_count INTEGER NOT NULL DEFAULT 0,
                error_count INTEGER NOT NULL DEFAULT 0,
                last_scanned_at TEXT,
                readonly INTEGER NOT NULL CHECK (readonly = 1),
                user_disabled INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS files (
                file_id TEXT PRIMARY KEY,
                canonical_path TEXT NOT NULL,
                path_key TEXT NOT NULL UNIQUE,
                name TEXT NOT NULL,
                extension TEXT NOT NULL,
                size_bytes INTEGER NOT NULL,
                modified_at TEXT NOT NULL,
                discovered_at TEXT NOT NULL,
                availability TEXT NOT NULL DEFAULT 'present'
            );

            CREATE TABLE IF NOT EXISTS file_root_memberships (
                file_id TEXT NOT NULL REFERENCES files(file_id) ON DELETE CASCADE,
                root_id TEXT NOT NULL REFERENCES roots(root_id) ON DELETE CASCADE,
                PRIMARY KEY (file_id, root_id)
            );

            CREATE TABLE IF NOT EXISTS jobs (
                job_id TEXT PRIMARY KEY,
                job_type TEXT NOT NULL,
                root_id TEXT REFERENCES roots(root_id) ON DELETE SET NULL,
                reason TEXT NOT NULL,
                status TEXT NOT NULL,
                stage TEXT NOT NULL,
                progress REAL NOT NULL,
                processed_items INTEGER NOT NULL,
                total_items INTEGER NOT NULL,
                error_json TEXT,
                created_at TEXT NOT NULL,
                started_at TEXT,
                finished_at TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_files_modified_at ON files(modified_at DESC);
            CREATE INDEX IF NOT EXISTS idx_memberships_root ON file_root_memberships(root_id);
            CREATE INDEX IF NOT EXISTS idx_jobs_root_created ON jobs(root_id, created_at DESC);
            CREATE UNIQUE INDEX IF NOT EXISTS idx_jobs_active_scan
                ON jobs(root_id)
                WHERE job_type = 'initial_scan' AND status IN ('queued', 'running');
        "#,
    },
    Migration {
        version: 2,
        name: "execution_contracts",
        sql: r#"
            CREATE TABLE IF NOT EXISTS execution_units (
                unit_id TEXT PRIMARY KEY,
                job_id TEXT NOT NULL REFERENCES jobs(job_id) ON DELETE CASCADE,
                unit_type TEXT NOT NULL,
                status TEXT NOT NULL,
                idempotency_key TEXT NOT NULL UNIQUE,
                contract_json TEXT NOT NULL,
                attempt_count INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL,
                started_at TEXT,
                finished_at TEXT
            );

            CREATE TABLE IF NOT EXISTS validation_checkpoints (
                checkpoint_id TEXT PRIMARY KEY,
                job_id TEXT NOT NULL REFERENCES jobs(job_id) ON DELETE CASCADE,
                unit_id TEXT NOT NULL REFERENCES execution_units(unit_id) ON DELETE CASCADE,
                checkpoint_type TEXT NOT NULL,
                status TEXT NOT NULL,
                rules_version TEXT NOT NULL,
                metrics_json TEXT NOT NULL,
                error_json TEXT,
                resume_token TEXT,
                created_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS exploration_candidates (
                candidate_id TEXT PRIMARY KEY,
                job_id TEXT NOT NULL REFERENCES jobs(job_id) ON DELETE CASCADE,
                strategy TEXT NOT NULL,
                status TEXT NOT NULL,
                candidate_json TEXT NOT NULL,
                created_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS degradation_states (
                state_id INTEGER PRIMARY KEY CHECK (state_id = 1),
                state_json TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_execution_units_job ON execution_units(job_id, created_at);
            CREATE INDEX IF NOT EXISTS idx_checkpoints_unit ON validation_checkpoints(unit_id, created_at);
            CREATE INDEX IF NOT EXISTS idx_candidates_job ON exploration_candidates(job_id, created_at);
        "#,
    },
    Migration {
        version: 3,
        name: "stable_catalog_and_recovery",
        sql: r#"
            ALTER TABLE roots ADD COLUMN path TEXT NOT NULL DEFAULT '';
            ALTER TABLE roots ADD COLUMN root_file_id TEXT;
            ALTER TABLE roots ADD COLUMN volume_id TEXT NOT NULL DEFAULT 'unknown';
            ALTER TABLE roots ADD COLUMN volume_type TEXT NOT NULL DEFAULT 'fixed';
            ALTER TABLE roots ADD COLUMN authorization_source TEXT NOT NULL DEFAULT 'system_default';
            ALTER TABLE roots ADD COLUMN root_kind TEXT NOT NULL DEFAULT 'known_folder';
            ALTER TABLE roots ADD COLUMN enabled INTEGER NOT NULL DEFAULT 1;
            ALTER TABLE roots ADD COLUMN watch_mode TEXT NOT NULL DEFAULT 'realtime';
            ALTER TABLE roots ADD COLUMN coverage_parent_root_id TEXT REFERENCES roots(root_id) ON DELETE SET NULL;
            ALTER TABLE roots ADD COLUMN permission_error_count INTEGER NOT NULL DEFAULT 0;
            ALTER TABLE roots ADD COLUMN last_scan_at TEXT;

            UPDATE roots
               SET path = canonical_path,
                   authorization_source = CASE source
                       WHEN 'user_folder' THEN 'user_selected'
                       WHEN 'candidate' THEN 'candidate_confirmed'
                       ELSE 'system_default'
                   END,
                   root_kind = CASE source
                       WHEN 'user_folder' THEN 'folder'
                       WHEN 'volume' THEN 'volume_root'
                       WHEN 'candidate' THEN 'app_candidate'
                       ELSE 'known_folder'
                   END,
                   enabled = CASE user_disabled WHEN 0 THEN 1 ELSE 0 END,
                   permission_error_count = error_count,
                   last_scan_at = last_scanned_at;

            ALTER TABLE files ADD COLUMN volume_id TEXT NOT NULL DEFAULT 'unknown';
            ALTER TABLE files ADD COLUMN display_name TEXT NOT NULL DEFAULT '';
            ALTER TABLE files ADD COLUMN mime_type TEXT NOT NULL DEFAULT 'application/octet-stream';
            ALTER TABLE files ADD COLUMN fs_created_at TEXT;
            ALTER TABLE files ADD COLUMN windows_file_id TEXT;
            ALTER TABLE files ADD COLUMN content_sha256 TEXT;
            ALTER TABLE files ADD COLUMN current_revision_id TEXT;
            ALTER TABLE files ADD COLUMN parse_status TEXT NOT NULL DEFAULT 'pending';
            ALTER TABLE files ADD COLUMN first_seen_at TEXT;
            ALTER TABLE files ADD COLUMN last_seen_at TEXT;

            UPDATE files
               SET display_name = name,
                   first_seen_at = discovered_at,
                   last_seen_at = discovered_at;

            ALTER TABLE file_root_memberships ADD COLUMN relative_path TEXT NOT NULL DEFAULT '';
            ALTER TABLE file_root_memberships ADD COLUMN is_primary INTEGER NOT NULL DEFAULT 0;

            CREATE TABLE file_revisions (
                revision_id TEXT PRIMARY KEY,
                file_id TEXT NOT NULL REFERENCES files(file_id) ON DELETE CASCADE,
                size_bytes INTEGER NOT NULL,
                fs_modified_at TEXT NOT NULL,
                content_sha256 TEXT,
                metadata_fingerprint TEXT NOT NULL,
                created_at TEXT NOT NULL,
                UNIQUE(file_id, metadata_fingerprint)
            );

            CREATE TABLE exclusion_rules (
                rule_id TEXT PRIMARY KEY,
                built_in_key TEXT UNIQUE,
                root_id TEXT REFERENCES roots(root_id) ON DELETE CASCADE,
                rule_class TEXT NOT NULL,
                rule_type TEXT NOT NULL,
                value_json TEXT NOT NULL,
                enabled INTEGER NOT NULL,
                overridable INTEGER NOT NULL,
                created_at TEXT NOT NULL
            );

            CREATE TABLE candidate_roots (
                candidate_id TEXT PRIMARY KEY,
                candidate_type TEXT NOT NULL,
                canonical_path TEXT NOT NULL,
                path_key TEXT NOT NULL UNIQUE,
                state TEXT NOT NULL,
                discovered_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE TABLE file_events (
                event_id TEXT PRIMARY KEY,
                event_fingerprint TEXT NOT NULL UNIQUE,
                root_id TEXT NOT NULL REFERENCES roots(root_id) ON DELETE CASCADE,
                event_type TEXT NOT NULL,
                observed_path TEXT NOT NULL,
                previous_path TEXT,
                observed_at TEXT NOT NULL,
                coalesced_at TEXT,
                state TEXT NOT NULL DEFAULT 'pending',
                error_json TEXT
            );

            CREATE TABLE log_events (
                log_id TEXT PRIMARY KEY,
                level TEXT NOT NULL,
                component TEXT NOT NULL,
                event_name TEXT NOT NULL,
                job_id TEXT,
                root_id TEXT,
                file_id TEXT,
                fields_json TEXT NOT NULL,
                created_at TEXT NOT NULL
            );

            ALTER TABLE jobs ADD COLUMN last_heartbeat_at TEXT;
            ALTER TABLE jobs ADD COLUMN resume_count INTEGER NOT NULL DEFAULT 0;
            ALTER TABLE jobs ADD COLUMN resume_token TEXT;

            CREATE UNIQUE INDEX idx_files_stable_identity
                ON files(volume_id, windows_file_id)
                WHERE windows_file_id IS NOT NULL;
            CREATE INDEX idx_files_availability ON files(availability, last_seen_at);
            CREATE INDEX idx_revisions_file ON file_revisions(file_id, created_at DESC);
            CREATE INDEX idx_file_events_state_time ON file_events(state, observed_at);
            CREATE INDEX idx_logs_created ON log_events(created_at DESC);
        "#,
    },
    Migration {
        version: 4,
        name: "document_index_foundation",
        sql: r#"
            ALTER TABLE file_revisions ADD COLUMN parse_status TEXT NOT NULL DEFAULT 'pending';
            ALTER TABLE file_revisions ADD COLUMN parser_name TEXT;
            ALTER TABLE file_revisions ADD COLUMN parser_version TEXT;
            ALTER TABLE file_revisions ADD COLUMN index_version INTEGER;
            ALTER TABLE file_revisions ADD COLUMN completed_at TEXT;
            ALTER TABLE file_revisions ADD COLUMN error_code TEXT;

            CREATE TABLE document_nodes (
                node_id TEXT PRIMARY KEY,
                revision_id TEXT NOT NULL REFERENCES file_revisions(revision_id) ON DELETE CASCADE,
                parent_id TEXT,
                ordinal INTEGER NOT NULL,
                node_type TEXT NOT NULL,
                locator_json TEXT NOT NULL,
                heading_path_json TEXT NOT NULL,
                text TEXT,
                table_json TEXT
            );

            CREATE TABLE chunks (
                chunk_id TEXT PRIMARY KEY,
                file_id TEXT NOT NULL REFERENCES files(file_id) ON DELETE CASCADE,
                revision_id TEXT NOT NULL REFERENCES file_revisions(revision_id) ON DELETE CASCADE,
                node_id TEXT NOT NULL REFERENCES document_nodes(node_id) ON DELETE CASCADE,
                ordinal INTEGER NOT NULL,
                text TEXT NOT NULL,
                normalized_text TEXT NOT NULL,
                token_count INTEGER NOT NULL,
                content_hash TEXT NOT NULL,
                language TEXT NOT NULL,
                locator_json TEXT NOT NULL,
                vector_key TEXT,
                embedding_model_id TEXT,
                embedding_status TEXT NOT NULL DEFAULT 'pending'
            );

            CREATE VIRTUAL TABLE chunks_fts USING fts5(
                chunk_id UNINDEXED,
                file_id UNINDEXED,
                revision_id UNINDEXED,
                normalized_text,
                tokenize = 'unicode61 remove_diacritics 2'
            );

            CREATE INDEX idx_nodes_revision_ordinal ON document_nodes(revision_id, ordinal);
            CREATE INDEX idx_chunks_revision_ordinal ON chunks(revision_id, ordinal);
            CREATE INDEX idx_chunks_file ON chunks(file_id, revision_id);
            CREATE INDEX idx_revisions_parse_status ON file_revisions(parse_status, created_at);
        "#,
    },
    Migration {
        version: 5,
        name: "inbox_collections_and_relations",
        sql: r#"
            CREATE TABLE inbox_events (
                inbox_id TEXT PRIMARY KEY,
                dedupe_key TEXT NOT NULL UNIQUE,
                file_id TEXT NOT NULL REFERENCES files(file_id) ON DELETE CASCADE,
                event_type TEXT NOT NULL,
                observed_at TEXT NOT NULL,
                previous_path TEXT,
                triage_status TEXT NOT NULL,
                summary TEXT,
                error_code TEXT,
                processed_at TEXT
            );

            CREATE TABLE collections (
                collection_id TEXT PRIMARY KEY,
                name TEXT NOT NULL COLLATE NOCASE UNIQUE,
                description TEXT,
                icon TEXT NOT NULL,
                color TEXT NOT NULL,
                kind TEXT NOT NULL,
                rule_json TEXT,
                built_in INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE TABLE collection_memberships (
                collection_id TEXT NOT NULL REFERENCES collections(collection_id) ON DELETE CASCADE,
                file_id TEXT NOT NULL REFERENCES files(file_id) ON DELETE CASCADE,
                source TEXT NOT NULL,
                created_at TEXT NOT NULL,
                PRIMARY KEY (collection_id, file_id)
            );

            CREATE TABLE file_relations (
                relation_id TEXT PRIMARY KEY,
                left_file_id TEXT NOT NULL REFERENCES files(file_id) ON DELETE CASCADE,
                right_file_id TEXT NOT NULL REFERENCES files(file_id) ON DELETE CASCADE,
                relation_type TEXT NOT NULL,
                confidence REAL NOT NULL,
                reasons_json TEXT NOT NULL,
                review_status TEXT NOT NULL DEFAULT 'suggested',
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                CHECK (left_file_id < right_file_id),
                UNIQUE (left_file_id, right_file_id, relation_type)
            );

            CREATE INDEX idx_inbox_status_time ON inbox_events(triage_status, observed_at DESC);
            CREATE INDEX idx_inbox_file ON inbox_events(file_id, observed_at DESC);
            CREATE INDEX idx_collection_memberships_file ON collection_memberships(file_id);
            CREATE INDEX idx_file_relations_type ON file_relations(relation_type, confidence DESC);
            CREATE INDEX idx_files_size_present ON files(size_bytes) WHERE availability = 'present';
        "#,
    },
    Migration {
        version: 6,
        name: "local_vector_index",
        sql: r#"
            CREATE TABLE chunk_embeddings (
                chunk_id TEXT NOT NULL REFERENCES chunks(chunk_id) ON DELETE CASCADE,
                model_artifact_id TEXT NOT NULL,
                file_id TEXT NOT NULL REFERENCES files(file_id) ON DELETE CASCADE,
                revision_id TEXT NOT NULL REFERENCES file_revisions(revision_id) ON DELETE CASCADE,
                dimension INTEGER NOT NULL,
                vector_blob BLOB NOT NULL,
                created_at TEXT NOT NULL,
                PRIMARY KEY (chunk_id, model_artifact_id)
            );

            CREATE INDEX idx_chunk_embeddings_model_file
                ON chunk_embeddings(model_artifact_id, file_id, revision_id);
        "#,
    },
];

#[derive(Debug, Clone)]
pub struct CatalogStore {
    database_path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct RootRegistration {
    pub label: String,
    pub canonical_path: String,
    pub path_key: String,
    pub source: RootSource,
    pub volume_id: String,
    pub root_file_id: Option<String>,
    pub authorization_source: AuthorizationSource,
    pub root_kind: RootKind,
    pub volume_type: VolumeType,
    pub watch_mode: WatchMode,
}

#[derive(Debug)]
pub struct LogEventInput<'a> {
    pub level: &'a str,
    pub component: &'a str,
    pub event_name: &'a str,
    pub job_id: Option<&'a Uuid>,
    pub root_id: Option<&'a Uuid>,
    pub file_id: Option<&'a Uuid>,
    pub fields: &'a serde_json::Value,
}

impl CatalogStore {
    pub fn open(database_path: impl Into<PathBuf>) -> Result<Self, AppError> {
        let store = Self {
            database_path: database_path.into(),
        };
        if let Some(parent) = store.database_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| AppError::local_config(error.to_string(), true))?;
        }
        let mut connection = store.connect()?;
        store.migrate(&mut connection)?;
        store.ensure_builtin_exclusion_rules()?;
        store.ensure_builtin_collections()?;
        store.backfill_inbox_events()?;
        Ok(store)
    }

    fn connect(&self) -> Result<Connection, AppError> {
        let connection = Connection::open(&self.database_path)
            .map_err(|error| storage_error("DATABASE_OPEN_FAILED", error, true))?;
        connection
            .busy_timeout(Duration::from_millis(250))
            .map_err(|error| storage_error("DATABASE_CONFIG_FAILED", error, true))?;
        connection
            .execute_batch("PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL;")
            .map_err(|error| storage_error("DATABASE_CONFIG_FAILED", error, true))?;
        Ok(connection)
    }

    fn migrate(&self, connection: &mut Connection) -> Result<(), AppError> {
        let current_version: u32 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(|error| storage_error("DATABASE_VERSION_READ_FAILED", error, false))?;
        if current_version > CURRENT_SCHEMA_VERSION {
            return Err(AppError::new(
                "DATABASE_SCHEMA_TOO_NEW",
                format!(
                    "数据库版本{current_version}高于当前程序支持的版本{CURRENT_SCHEMA_VERSION}"
                ),
                false,
            ));
        }
        connection
            .execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS schema_migrations (
                    version INTEGER PRIMARY KEY,
                    name TEXT NOT NULL,
                    applied_at TEXT NOT NULL
                );
                "#,
            )
            .map_err(|error| storage_error("DATABASE_MIGRATION_FAILED", error, false))?;

        for migration in MIGRATIONS
            .iter()
            .filter(|migration| migration.version <= current_version)
        {
            connection
                .execute(
                    "INSERT OR IGNORE INTO schema_migrations (version, name, applied_at) VALUES (?1, ?2, ?3)",
                    params![migration.version, migration.name, Utc::now().to_rfc3339()],
                )
                .map_err(|error| storage_error("DATABASE_MIGRATION_HISTORY_FAILED", error, false))?;
        }

        for migration in MIGRATIONS
            .iter()
            .filter(|migration| migration.version > current_version)
        {
            let transaction = connection
                .transaction()
                .map_err(|error| storage_error("DATABASE_TRANSACTION_FAILED", error, false))?;
            transaction
                .execute_batch(migration.sql)
                .map_err(|error| storage_error("DATABASE_MIGRATION_FAILED", error, false))?;
            transaction
                .execute(
                    "INSERT INTO schema_migrations (version, name, applied_at) VALUES (?1, ?2, ?3)",
                    params![migration.version, migration.name, Utc::now().to_rfc3339()],
                )
                .map_err(|error| {
                    storage_error("DATABASE_MIGRATION_HISTORY_FAILED", error, false)
                })?;
            transaction
                .pragma_update(None, "user_version", migration.version)
                .map_err(|error| storage_error("DATABASE_VERSION_WRITE_FAILED", error, false))?;
            transaction
                .commit()
                .map_err(|error| storage_error("DATABASE_COMMIT_FAILED", error, false))?;
        }
        Ok(())
    }

    pub fn schema_version(&self) -> Result<u32, AppError> {
        let connection = self.connect()?;
        connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(|error| storage_error("DATABASE_VERSION_READ_FAILED", error, false))
    }

    pub fn migration_history(&self) -> Result<Vec<(u32, String)>, AppError> {
        let connection = self.connect()?;
        let mut statement = connection
            .prepare("SELECT version, name FROM schema_migrations ORDER BY version")
            .map_err(|error| storage_error("DATABASE_MIGRATION_HISTORY_FAILED", error, false))?;
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .map_err(|error| storage_error("DATABASE_MIGRATION_HISTORY_FAILED", error, false))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| storage_error("DATABASE_MIGRATION_HISTORY_FAILED", error, false))
    }

    fn ensure_builtin_exclusion_rules(&self) -> Result<(), AppError> {
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction()
            .map_err(|error| storage_error("DATABASE_TRANSACTION_FAILED", error, true))?;
        for rule in BUILT_IN_EXCLUSION_RULES {
            transaction
                .execute(
                    "INSERT OR IGNORE INTO exclusion_rules (rule_id, built_in_key, root_id, rule_class, rule_type, value_json, enabled, overridable, created_at) VALUES (?1, ?2, NULL, ?3, ?4, ?5, 1, ?6, ?7)",
                    params![Uuid::now_v7().to_string(), rule.key, rule.rule_class.as_str(), rule.rule_type.as_str(), serde_json::to_string(rule.value).expect("static exclusion value serializes"), i64::from(rule.overridable), Utc::now().to_rfc3339()],
                )
                .map_err(|error| storage_error("EXCLUSION_RULE_SEED_FAILED", error, false))?;
        }
        transaction
            .commit()
            .map_err(|error| storage_error("DATABASE_COMMIT_FAILED", error, true))
    }

    fn ensure_builtin_collections(&self) -> Result<(), AppError> {
        let connection = self.connect()?;
        let now = Utc::now().to_rfc3339();
        let built_ins = [
            (
                "018f0000-0000-7000-8000-000000001001",
                "最近7天",
                "最近7天修改过的资料",
                "calendar",
                "#8c7cf0",
                serde_json::json!({
                    "operator": "all",
                    "extensions": [],
                    "filename_keywords": [],
                    "path_keywords": [],
                    "text_keywords": [],
                    "parse_statuses": [],
                    "modified_within_days": 7
                }),
            ),
            (
                "018f0000-0000-7000-8000-000000001002",
                "待处理资料",
                "等待解析、OCR或处理失败的资料",
                "pending",
                "#e7a6ba",
                serde_json::json!({
                    "operator": "any",
                    "extensions": [],
                    "filename_keywords": [],
                    "path_keywords": [],
                    "text_keywords": [],
                    "parse_statuses": ["pending", "parsing", "ocr_pending", "failed"],
                    "modified_within_days": null
                }),
            ),
            (
                "018f0000-0000-7000-8000-000000001003",
                "PDF资料",
                "全部可访问的PDF资料",
                "pdf",
                "#71a7ca",
                serde_json::json!({
                    "operator": "all",
                    "extensions": ["pdf"],
                    "filename_keywords": [],
                    "path_keywords": [],
                    "text_keywords": [],
                    "parse_statuses": [],
                    "modified_within_days": null
                }),
            ),
        ];
        for (collection_id, name, description, icon, color, rule) in built_ins {
            connection
                .execute(
                    "INSERT OR IGNORE INTO collections (collection_id, name, description, icon, color, kind, rule_json, built_in, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, 'rule', ?6, 1, ?7, ?7)",
                    params![collection_id, name, description, icon, color, rule.to_string(), now],
                )
                .map_err(|error| storage_error("COLLECTION_SEED_FAILED", error, false))?;
        }
        Ok(())
    }

    fn backfill_inbox_events(&self) -> Result<(), AppError> {
        let mut connection = self.connect()?;
        let pending = {
            let mut statement = connection
                .prepare(
                    "SELECT f.file_id, f.display_name, f.first_seen_at FROM files f WHERE NOT EXISTS (SELECT 1 FROM inbox_events i WHERE i.file_id = f.file_id) ORDER BY f.first_seen_at",
                )
                .map_err(|error| storage_error("INBOX_QUERY_FAILED", error, true))?;
            statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })
                .map_err(|error| storage_error("INBOX_QUERY_FAILED", error, true))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| storage_error("INBOX_QUERY_FAILED", error, true))?
        };
        if pending.is_empty() {
            return Ok(());
        }
        let transaction = connection
            .transaction()
            .map_err(|error| storage_error("DATABASE_TRANSACTION_FAILED", error, true))?;
        for (file_id, display_name, observed_at) in pending {
            transaction
                .execute(
                    "INSERT OR IGNORE INTO inbox_events (inbox_id, dedupe_key, file_id, event_type, observed_at, triage_status, summary) VALUES (?1, ?2, ?3, 'discovered', ?4, 'new', ?5)",
                    params![Uuid::now_v7().to_string(), format!("backfill:{file_id}"), file_id, observed_at, format!("已发现资料：{display_name}")],
                )
                .map_err(|error| storage_error("INBOX_WRITE_FAILED", error, true))?;
        }
        transaction
            .commit()
            .map_err(|error| storage_error("DATABASE_COMMIT_FAILED", error, true))
    }

    pub fn list_exclusion_rules(&self) -> Result<Vec<ExclusionRule>, AppError> {
        let connection = self.connect()?;
        let mut statement = connection
            .prepare("SELECT rule_id, root_id, rule_class, rule_type, value_json, enabled, overridable FROM exclusion_rules ORDER BY built_in_key, created_at")
            .map_err(|error| storage_error("EXCLUSION_RULE_QUERY_FAILED", error, true))?;
        statement
            .query_map([], |row| {
                let rule_id: String = row.get(0)?;
                let root_id: Option<String> = row.get(1)?;
                let rule_class: String = row.get(2)?;
                let rule_type: String = row.get(3)?;
                let value_json: String = row.get(4)?;
                Ok(ExclusionRule {
                    rule_id: parse_uuid_column(&rule_id, 0)?,
                    root_id: root_id
                        .map(|value| parse_uuid_column(&value, 1))
                        .transpose()?,
                    rule_class: ExclusionRuleClass::from_storage(&rule_class),
                    rule_type: ExclusionRuleType::from_storage(&rule_type),
                    value: serde_json::from_str(&value_json).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            4,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })?,
                    enabled: row.get::<_, i64>(5)? != 0,
                    overridable: row.get::<_, i64>(6)? != 0,
                })
            })
            .map_err(|error| storage_error("EXCLUSION_RULE_QUERY_FAILED", error, true))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| storage_error("EXCLUSION_RULE_QUERY_FAILED", error, true))
    }

    pub fn upsert_candidate_root(
        &self,
        candidate_type: CandidateRootType,
        canonical_path: &str,
        path_key: &str,
    ) -> Result<CandidateRoot, AppError> {
        let connection = self.connect()?;
        connection
            .execute(
                "INSERT INTO candidate_roots (candidate_id, candidate_type, canonical_path, path_key, state, discovered_at, updated_at) VALUES (?1, ?2, ?3, ?4, 'suggested', ?5, ?5) ON CONFLICT(path_key) DO UPDATE SET candidate_type = excluded.candidate_type, canonical_path = excluded.canonical_path, updated_at = excluded.updated_at",
                params![Uuid::now_v7().to_string(), candidate_type.as_str(), canonical_path, path_key, Utc::now().to_rfc3339()],
            )
            .map_err(|error| storage_error("CANDIDATE_UPSERT_FAILED", error, true))?;
        connection
            .query_row(
                "SELECT candidate_id, candidate_type, canonical_path, state FROM candidate_roots WHERE path_key = ?1",
                [path_key],
                candidate_from_row,
            )
            .map_err(|error| storage_error("CANDIDATE_QUERY_FAILED", error, true))
    }

    pub fn list_candidate_roots(&self) -> Result<Vec<CandidateRoot>, AppError> {
        let connection = self.connect()?;
        let mut statement = connection
            .prepare("SELECT candidate_id, candidate_type, canonical_path, state FROM candidate_roots ORDER BY discovered_at")
            .map_err(|error| storage_error("CANDIDATE_QUERY_FAILED", error, true))?;
        statement
            .query_map([], candidate_from_row)
            .map_err(|error| storage_error("CANDIDATE_QUERY_FAILED", error, true))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| storage_error("CANDIDATE_QUERY_FAILED", error, true))
    }

    pub fn candidate_root_by_id(&self, candidate_id: &Uuid) -> Result<CandidateRoot, AppError> {
        let connection = self.connect()?;
        connection
            .query_row(
                "SELECT candidate_id, candidate_type, canonical_path, state FROM candidate_roots WHERE candidate_id = ?1",
                [candidate_id.to_string()],
                candidate_from_row,
            )
            .map_err(|error| storage_error("CANDIDATE_NOT_FOUND", error, false))
    }

    pub fn update_candidate_root_status(
        &self,
        candidate_id: &Uuid,
        status: CandidateRootStatus,
    ) -> Result<CandidateRoot, AppError> {
        let connection = self.connect()?;
        let changed = connection
            .execute(
                "UPDATE candidate_roots SET state = ?1, updated_at = ?2 WHERE candidate_id = ?3",
                params![
                    status.as_str(),
                    Utc::now().to_rfc3339(),
                    candidate_id.to_string()
                ],
            )
            .map_err(|error| storage_error("CANDIDATE_UPDATE_FAILED", error, true))?;
        if changed == 0 {
            return Err(AppError::new(
                "CANDIDATE_NOT_FOUND",
                "候选资料来源不存在",
                false,
            ));
        }
        self.candidate_root_by_id(candidate_id)
    }

    pub fn upsert_root(&self, registration: &RootRegistration) -> Result<RootRecord, AppError> {
        let connection = self.connect()?;
        if let Some(existing) = self.root_by_path_key_with(&connection, &registration.path_key)? {
            if !existing.enabled
                && registration.authorization_source == AuthorizationSource::SystemDefault
            {
                return Ok(existing);
            }
            connection
                .execute(
                    "UPDATE roots SET label = ?1, path = ?2, canonical_path = ?2, source = ?3, volume_id = ?4, root_file_id = ?5, authorization_source = ?6, root_kind = ?7, volume_type = ?8, watch_mode = ?9, enabled = 1, user_disabled = 0 WHERE root_id = ?10",
                    params![registration.label, registration.canonical_path, registration.source.as_str(), registration.volume_id, registration.root_file_id, registration.authorization_source.as_str(), registration.root_kind.as_str(), registration.volume_type.as_str(), registration.watch_mode.as_str(), existing.root_id.to_string()],
                )
                .map_err(|error| storage_error("ROOT_UPDATE_FAILED", error, true))?;
            refresh_root_coverage(&connection)?;
            return self.root_by_id(&existing.root_id);
        }

        let root_id = Uuid::now_v7();
        connection
            .execute(
                "INSERT INTO roots (root_id, label, path, canonical_path, path_key, source, status, readonly, created_at, root_file_id, volume_id, volume_type, authorization_source, root_kind, enabled, watch_mode, permission_error_count) VALUES (?1, ?2, ?3, ?3, ?4, ?5, 'ready', 1, ?6, ?7, ?8, ?9, ?10, ?11, 1, ?12, 0)",
                params![
                    root_id.to_string(),
                    registration.label,
                    registration.canonical_path,
                    registration.path_key,
                    registration.source.as_str(),
                    Utc::now().to_rfc3339(),
                    registration.root_file_id,
                    registration.volume_id,
                    registration.volume_type.as_str(),
                    registration.authorization_source.as_str(),
                    registration.root_kind.as_str(),
                    registration.watch_mode.as_str(),
                ],
            )
            .map_err(|error| storage_error("ROOT_INSERT_FAILED", error, true))?;
        refresh_root_coverage(&connection)?;
        self.root_by_id(&root_id)
    }

    pub fn list_roots(&self) -> Result<Vec<RootRecord>, AppError> {
        let connection = self.connect()?;
        let mut statement = connection
            .prepare(&format!(
                "{ROOT_SELECT} WHERE enabled = 1 ORDER BY created_at"
            ))
            .map_err(|error| storage_error("ROOT_QUERY_FAILED", error, true))?;
        let roots = statement
            .query_map([], root_from_row)
            .map_err(|error| storage_error("ROOT_QUERY_FAILED", error, true))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| storage_error("ROOT_QUERY_FAILED", error, true))?;
        Ok(roots)
    }

    pub fn disable_root(&self, root_id: &Uuid) -> Result<(), AppError> {
        let connection = self.connect()?;
        let transaction = connection
            .unchecked_transaction()
            .map_err(|error| storage_error("DATABASE_TRANSACTION_FAILED", error, true))?;
        let changed = transaction
            .execute(
                "UPDATE roots SET enabled = 0, user_disabled = 1, status = 'paused' WHERE root_id = ?1 AND enabled = 1",
                [root_id.to_string()],
            )
            .map_err(|error| storage_error("ROOT_DISABLE_FAILED", error, true))?;
        if changed == 0 {
            return Err(AppError::new(
                "ROOT_NOT_FOUND",
                "资料位置不存在或已经停用",
                false,
            ));
        }
        transaction
            .execute(
                "DELETE FROM file_root_memberships WHERE root_id = ?1",
                [root_id.to_string()],
            )
            .map_err(|error| storage_error("MEMBERSHIP_DELETE_FAILED", error, true))?;
        transaction
            .execute(
                "DELETE FROM files WHERE NOT EXISTS (SELECT 1 FROM file_root_memberships m JOIN roots r ON r.root_id = m.root_id WHERE m.file_id = files.file_id AND r.enabled = 1)",
                [],
            )
            .map_err(|error| storage_error("ROOT_INDEX_PURGE_FAILED", error, true))?;
        transaction
            .execute("UPDATE file_root_memberships SET is_primary = 0", [])
            .map_err(|error| storage_error("MEMBERSHIP_UPDATE_FAILED", error, true))?;
        transaction
            .execute(
                "UPDATE file_root_memberships SET is_primary = 1 WHERE rowid IN (SELECT MIN(rowid) FROM file_root_memberships GROUP BY file_id)",
                [],
            )
            .map_err(|error| storage_error("MEMBERSHIP_UPDATE_FAILED", error, true))?;
        refresh_root_coverage(&transaction)?;
        transaction
            .commit()
            .map_err(|error| storage_error("DATABASE_COMMIT_FAILED", error, true))?;
        Ok(())
    }

    pub fn root_by_id(&self, root_id: &Uuid) -> Result<RootRecord, AppError> {
        let connection = self.connect()?;
        connection
            .query_row(
                &format!("{ROOT_SELECT} WHERE root_id = ?1 AND enabled = 1"),
                [root_id.to_string()],
                root_from_row,
            )
            .map_err(|error| storage_error("ROOT_NOT_FOUND", error, false))
    }

    fn root_by_path_key_with(
        &self,
        connection: &Connection,
        path_key: &str,
    ) -> Result<Option<RootRecord>, AppError> {
        connection
            .query_row(
                &format!("{ROOT_SELECT} WHERE path_key = ?1"),
                [path_key],
                root_from_row,
            )
            .optional()
            .map_err(|error| storage_error("ROOT_QUERY_FAILED", error, true))
    }

    pub fn prepare_scan_job(
        &self,
        root_id: &Uuid,
        reason: &str,
    ) -> Result<(JobRecord, bool), AppError> {
        self.root_by_id(root_id)?;
        let connection = self.connect()?;
        if let Some(job) = active_scan_job_with(&connection, root_id)? {
            return Ok((job, false));
        }
        let job = JobRecord {
            job_id: Uuid::now_v7(),
            job_type: "initial_scan".to_owned(),
            status: JobStatus::Queued,
            stage: "queued".to_owned(),
            progress: 0.0,
            processed_items: 0,
            total_items: 0,
            error: None,
            created_at: Utc::now(),
            started_at: None,
            finished_at: None,
        };
        connection
            .execute(
                "INSERT INTO jobs (job_id, job_type, root_id, reason, status, stage, progress, processed_items, total_items, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    job.job_id.to_string(),
                    job.job_type,
                    root_id.to_string(),
                    reason,
                    job.status.as_str(),
                    job.stage,
                    job.progress,
                    job.processed_items,
                    job.total_items,
                    job.created_at.to_rfc3339(),
                ],
            )
            .map_err(|error| storage_error("JOB_INSERT_FAILED", error, true))?;
        Ok((job, true))
    }

    pub fn job_by_id(&self, job_id: &Uuid) -> Result<JobRecord, AppError> {
        let connection = self.connect()?;
        connection
            .query_row(
                "SELECT job_id, job_type, status, stage, progress, processed_items, total_items, error_json, created_at, started_at, finished_at FROM jobs WHERE job_id = ?1",
                [job_id.to_string()],
                job_from_row,
            )
            .map_err(|error| storage_error("JOB_NOT_FOUND", error, false))
    }

    pub fn begin_task(&self, plan: &TaskPlan) -> Result<JobRecord, AppError> {
        let connection = self.connect()?;
        let transaction = connection
            .unchecked_transaction()
            .map_err(|error| storage_error("DATABASE_TRANSACTION_FAILED", error, true))?;
        let now = Utc::now();
        let job = JobRecord {
            job_id: plan.task_id,
            job_type: format!("task.{}", plan.skill_id),
            status: JobStatus::Running,
            stage: plan
                .steps
                .first()
                .map(|step| step.step_type.clone())
                .unwrap_or_else(|| "running".into()),
            progress: 0.0,
            processed_items: 0,
            total_items: plan.steps.len() as u64,
            error: None,
            created_at: now,
            started_at: Some(now),
            finished_at: None,
        };
        transaction
            .execute(
                "INSERT INTO jobs (job_id, job_type, root_id, reason, status, stage, progress, processed_items, total_items, created_at, started_at, last_heartbeat_at, resume_token) VALUES (?1, ?2, NULL, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9, ?9, ?10)",
                params![job.job_id.to_string(), job.job_type, plan.summary, job.status.as_str(), job.stage, job.progress, job.processed_items, job.total_items, now.to_rfc3339(), format!("{}:0", plan.task_id)],
            )
            .map_err(|error| storage_error("JOB_INSERT_FAILED", error, true))?;
        for step in &plan.steps {
            let contract = serde_json::to_string(step).map_err(|error| {
                AppError::new("TASK_PLAN_SERIALIZE_FAILED", error.to_string(), false)
            })?;
            transaction
                .execute(
                    "INSERT INTO execution_units (unit_id, job_id, unit_type, status, idempotency_key, contract_json, attempt_count, created_at) VALUES (?1, ?2, ?3, 'pending', ?4, ?5, 0, ?6)",
                    params![step.step_id.to_string(), plan.task_id.to_string(), step.step_type, format!("{}:{}", plan.task_id, step.ordinal), contract, now.to_rfc3339()],
                )
                .map_err(|error| storage_error("TASK_UNIT_INSERT_FAILED", error, true))?;
        }
        transaction
            .commit()
            .map_err(|error| storage_error("DATABASE_COMMIT_FAILED", error, true))?;
        Ok(job)
    }

    pub fn task_plan_by_id(&self, task_id: &Uuid) -> Result<Option<TaskPlan>, AppError> {
        let connection = self.connect()?;
        let task = connection
            .query_row(
                "SELECT job_type, reason FROM jobs WHERE job_id = ?1 AND job_type LIKE 'task.%'",
                [task_id.to_string()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(|error| storage_error("JOB_QUERY_FAILED", error, true))?;
        let Some((job_type, summary)) = task else {
            return Ok(None);
        };
        let mut statement = connection
            .prepare("SELECT contract_json FROM execution_units WHERE job_id = ?1")
            .map_err(|error| storage_error("TASK_UNIT_QUERY_FAILED", error, true))?;
        let mut steps = statement
            .query_map([task_id.to_string()], |row| row.get::<_, String>(0))
            .map_err(|error| storage_error("TASK_UNIT_QUERY_FAILED", error, true))?
            .map(|value| {
                serde_json::from_str::<TaskStep>(
                    &value.map_err(|error| storage_error("TASK_UNIT_QUERY_FAILED", error, true))?,
                )
                .map_err(|error| {
                    AppError::new("TASK_PLAN_DESERIALIZE_FAILED", error.to_string(), false)
                })
            })
            .collect::<Result<Vec<_>, AppError>>()?;
        steps.sort_by_key(|step| step.ordinal);
        let estimated_file_count = steps
            .first()
            .and_then(|step| step.inputs.get("file_ids"))
            .and_then(|value| value.as_array())
            .map_or(0, |items| items.len() as u64);
        Ok(Some(TaskPlan {
            task_id: *task_id,
            skill_id: job_type.trim_start_matches("task.").to_owned(),
            skill_version: "1.0.0".into(),
            summary,
            steps,
            estimated_file_count,
            warnings: vec![
                "这是从本地检查点恢复的任务；已通过的原子步骤不会重复执行。".into(),
                "任务只读取源文件；结果仍需复核并显式导出。".into(),
            ],
        }))
    }

    pub fn resume_task(&self, task_id: &Uuid) -> Result<JobRecord, AppError> {
        let connection = self.connect()?;
        let (status, resume_count): (String, u32) = connection
            .query_row(
                "SELECT status, resume_count FROM jobs WHERE job_id = ?1 AND job_type LIKE 'task.%'",
                [task_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|error| storage_error("JOB_QUERY_FAILED", error, true))?
            .ok_or_else(|| AppError::new("TASK_JOB_NOT_FOUND", "待恢复任务不存在", false))?;
        if matches!(status.as_str(), "succeeded" | "partial" | "cancelled") {
            return Err(AppError::new(
                "TASK_JOB_TERMINAL",
                "任务已经结束，不能重复执行",
                false,
            ));
        }
        if resume_count >= 3 {
            return Err(AppError::new(
                "TASK_RECOVERY_LIMIT_REACHED",
                "任务已连续恢复三次，请重新创建任务",
                false,
            ));
        }
        let next_step = connection
            .query_row(
                "SELECT unit_type FROM execution_units WHERE job_id = ?1 AND status <> 'succeeded' ORDER BY created_at LIMIT 1",
                [task_id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| storage_error("TASK_UNIT_QUERY_FAILED", error, true))?
            .unwrap_or_else(|| "result.review".into());
        let now = Utc::now().to_rfc3339();
        connection
            .execute(
                "UPDATE jobs SET status = 'running', stage = ?1, error_json = NULL, finished_at = NULL, started_at = COALESCE(started_at, ?2), last_heartbeat_at = ?2, resume_count = resume_count + 1 WHERE job_id = ?3",
                params![next_step, now, task_id.to_string()],
            )
            .map_err(|error| storage_error("JOB_UPDATE_FAILED", error, true))?;
        self.job_by_id(task_id)
    }

    pub fn task_checkpoints(&self, task_id: &Uuid) -> Result<Vec<ValidationCheckpoint>, AppError> {
        let connection = self.connect()?;
        let mut statement = connection
            .prepare(
                "SELECT checkpoint_id, unit_id, checkpoint_type, status, rules_version, metrics_json, error_json, resume_token, created_at FROM validation_checkpoints WHERE job_id = ?1 ORDER BY created_at",
            )
            .map_err(|error| storage_error("TASK_CHECKPOINT_QUERY_FAILED", error, true))?;
        statement
            .query_map([task_id.to_string()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, String>(8)?,
                ))
            })
            .map_err(|error| storage_error("TASK_CHECKPOINT_QUERY_FAILED", error, true))?
            .map(|row| {
                let (checkpoint_id, unit_id, kind, status, rules, metrics, error, resume, created) =
                    row.map_err(|error| {
                        storage_error("TASK_CHECKPOINT_QUERY_FAILED", error, true)
                    })?;
                Ok(ValidationCheckpoint {
                    checkpoint_id: parse_uuid_value(&checkpoint_id)?,
                    job_id: *task_id,
                    unit_id: parse_uuid_value(&unit_id)?,
                    checkpoint_type: checkpoint_type_from_str(&kind)?,
                    status: checkpoint_status_from_str(&status)?,
                    rules_version: rules,
                    metrics: serde_json::from_str(&metrics).map_err(|error| {
                        AppError::new("TASK_CHECKPOINT_DATA_INVALID", error.to_string(), false)
                    })?,
                    error: error
                        .map(|value| serde_json::from_str(&value))
                        .transpose()
                        .map_err(|error| {
                            AppError::new("TASK_CHECKPOINT_DATA_INVALID", error.to_string(), false)
                        })?,
                    created_at: parse_datetime_value(&created)?,
                    resume_token: resume,
                })
            })
              .collect()
    }

    pub fn replace_task_exploration_candidates(
        &self,
        task_id: &Uuid,
        candidates: &[ExplorationCandidate],
    ) -> Result<(), AppError> {
        if !(2..=3).contains(&candidates.len())
            || candidates
                .iter()
                .any(|candidate| candidate.job_id != *task_id)
        {
            return Err(AppError::new(
                "TASK_CANDIDATE_DATA_INVALID",
                "多路径探索必须记录同一任务的2到3条候选路径",
                false,
            ));
        }
        for candidate in candidates {
            candidate.validate()?;
        }
        let connection = self.connect()?;
        let transaction = connection
            .unchecked_transaction()
            .map_err(|error| storage_error("DATABASE_TRANSACTION_FAILED", error, true))?;
        transaction
            .execute(
                "DELETE FROM exploration_candidates WHERE job_id = ?1",
                [task_id.to_string()],
            )
            .map_err(|error| storage_error("TASK_CANDIDATE_WRITE_FAILED", error, true))?;
        let now = Utc::now().to_rfc3339();
        for candidate in candidates {
            let candidate_json = serde_json::to_string(candidate).map_err(|error| {
                AppError::new("TASK_CANDIDATE_DATA_INVALID", error.to_string(), false)
            })?;
            transaction
                .execute(
                    "INSERT INTO exploration_candidates (candidate_id, job_id, strategy, status, candidate_json, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        candidate.candidate_id.to_string(),
                        task_id.to_string(),
                        candidate.strategy.as_str(),
                        serde_json::to_value(candidate.status)
                            .ok()
                            .and_then(|value| value.as_str().map(ToOwned::to_owned))
                            .unwrap_or_else(|| "pending".into()),
                        candidate_json,
                        now,
                    ],
                )
                .map_err(|error| storage_error("TASK_CANDIDATE_WRITE_FAILED", error, true))?;
        }
        transaction
            .commit()
            .map_err(|error| storage_error("DATABASE_TRANSACTION_FAILED", error, true))
    }

    pub fn task_exploration_candidates(
        &self,
        task_id: &Uuid,
    ) -> Result<Vec<ExplorationCandidate>, AppError> {
        let connection = self.connect()?;
        let mut statement = connection
            .prepare(
                "SELECT candidate_json FROM exploration_candidates WHERE job_id = ?1 ORDER BY created_at, candidate_id",
            )
            .map_err(|error| storage_error("TASK_CANDIDATE_QUERY_FAILED", error, true))?;
        statement
            .query_map([task_id.to_string()], |row| row.get::<_, String>(0))
            .map_err(|error| storage_error("TASK_CANDIDATE_QUERY_FAILED", error, true))?
            .map(|row| {
                let value =
                    row.map_err(|error| storage_error("TASK_CANDIDATE_QUERY_FAILED", error, true))?;
                let candidate =
                    serde_json::from_str::<ExplorationCandidate>(&value).map_err(|error| {
                        AppError::new("TASK_CANDIDATE_DATA_INVALID", error.to_string(), false)
                    })?;
                candidate.validate()?;
                Ok(candidate)
            })
            .collect()
    }

    pub fn recover_interrupted_tasks(&self) -> Result<u64, AppError> {
        let connection = self.connect()?;
        connection
            .execute(
                "UPDATE jobs SET status = 'paused', stage = 'recovery_pending', last_heartbeat_at = ?1 WHERE job_type LIKE 'task.%' AND status = 'running'",
                [Utc::now().to_rfc3339()],
            )
            .map(|count| count as u64)
            .map_err(|error| storage_error("TASK_RECOVERY_UPDATE_FAILED", error, true))
    }

    pub fn latest_recoverable_task_plan(&self) -> Result<Option<TaskPlan>, AppError> {
        let connection = self.connect()?;
        let task_id = connection
            .query_row(
                "SELECT job_id FROM jobs WHERE job_type LIKE 'task.%' AND status IN ('paused', 'failed') AND resume_count < 3 ORDER BY created_at DESC LIMIT 1",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| storage_error("JOB_QUERY_FAILED", error, true))?;
        task_id
            .map(|value| parse_uuid_value(&value))
            .transpose()?
            .map(|task_id| self.task_plan_by_id(&task_id))
            .transpose()
            .map(Option::flatten)
    }

    pub fn pass_task_step(
        &self,
        job_id: &Uuid,
        step: &TaskStep,
        checkpoint_type: CheckpointType,
        metrics: serde_json::Value,
    ) -> Result<ValidationCheckpoint, AppError> {
        let connection = self.connect()?;
        let transaction = connection
            .unchecked_transaction()
            .map_err(|error| storage_error("DATABASE_TRANSACTION_FAILED", error, true))?;
        let now = Utc::now();
        let changed = transaction
            .execute(
                "UPDATE execution_units SET status = 'succeeded', attempt_count = attempt_count + 1, started_at = COALESCE(started_at, ?1), finished_at = ?1 WHERE unit_id = ?2 AND job_id = ?3 AND status = 'pending'",
                params![now.to_rfc3339(), step.step_id.to_string(), job_id.to_string()],
            )
            .map_err(|error| storage_error("TASK_UNIT_UPDATE_FAILED", error, true))?;
        if changed == 0 {
            return Err(AppError::new(
                "TASK_UNIT_STATE_INVALID",
                "任务步骤不处于待执行状态",
                false,
            ));
        }
        let checkpoint = ValidationCheckpoint {
            checkpoint_id: Uuid::now_v7(),
            job_id: *job_id,
            unit_id: step.step_id,
            checkpoint_type,
            status: CheckpointStatus::Passed,
            rules_version: "1.0.0".into(),
            metrics,
            error: None,
            created_at: now,
            resume_token: Some(format!("{}:{}", job_id, step.ordinal)),
        };
        checkpoint.validate()?;
        transaction
            .execute(
                "INSERT INTO validation_checkpoints (checkpoint_id, job_id, unit_id, checkpoint_type, status, rules_version, metrics_json, error_json, resume_token, created_at) VALUES (?1, ?2, ?3, ?4, 'passed', ?5, ?6, NULL, ?7, ?8)",
                params![checkpoint.checkpoint_id.to_string(), job_id.to_string(), step.step_id.to_string(), checkpoint_type_as_str(checkpoint_type), checkpoint.rules_version, serde_json::to_string(&checkpoint.metrics).map_err(|error| AppError::new("TASK_CHECKPOINT_SERIALIZE_FAILED", error.to_string(), false))?, checkpoint.resume_token, now.to_rfc3339()],
            )
            .map_err(|error| storage_error("TASK_CHECKPOINT_INSERT_FAILED", error, true))?;
        let completed = step.ordinal as u64;
        let total = transaction
            .query_row(
                "SELECT total_items FROM jobs WHERE job_id = ?1",
                [job_id.to_string()],
                |row| row.get::<_, u64>(0),
            )
            .map_err(|error| storage_error("JOB_QUERY_FAILED", error, true))?;
        let progress = if total == 0 {
            1.0
        } else {
            completed as f64 / total as f64
        };
        transaction
            .execute(
                "UPDATE jobs SET stage = ?1, processed_items = ?2, progress = ?3, last_heartbeat_at = ?4, resume_token = ?5 WHERE job_id = ?6 AND status = 'running'",
                params![step.step_type, completed, progress, now.to_rfc3339(), checkpoint.resume_token, job_id.to_string()],
            )
            .map_err(|error| storage_error("JOB_UPDATE_FAILED", error, true))?;
        transaction
            .commit()
            .map_err(|error| storage_error("DATABASE_COMMIT_FAILED", error, true))?;
        Ok(checkpoint)
    }

    pub fn finish_task(&self, job_id: &Uuid) -> Result<JobRecord, AppError> {
        let connection = self.connect()?;
        let now = Utc::now();
        connection
            .execute(
                "UPDATE jobs SET status = 'succeeded', stage = 'completed', progress = 1.0, processed_items = total_items, finished_at = ?1, last_heartbeat_at = ?1, resume_token = NULL WHERE job_id = ?2 AND status = 'running'",
                params![now.to_rfc3339(), job_id.to_string()],
            )
            .map_err(|error| storage_error("JOB_UPDATE_FAILED", error, true))?;
        self.job_by_id(job_id)
    }

    pub fn fail_task(&self, job_id: &Uuid, error: &AppError) -> Result<JobRecord, AppError> {
        let connection = self.connect()?;
        let now = Utc::now();
        let error_json = serde_json::to_string(error).map_err(|serialize_error| {
            AppError::new(
                "JOB_ERROR_SERIALIZE_FAILED",
                serialize_error.to_string(),
                false,
            )
        })?;
        connection
            .execute(
                "UPDATE jobs SET status = 'failed', stage = 'failed', error_json = ?1, finished_at = ?2, last_heartbeat_at = ?2 WHERE job_id = ?3 AND status = 'running'",
                params![error_json, now.to_rfc3339(), job_id.to_string()],
            )
            .map_err(|update_error| storage_error("JOB_UPDATE_FAILED", update_error, true))?;
        self.job_by_id(job_id)
    }

    pub fn latest_active_scan_job(&self) -> Result<Option<JobRecord>, AppError> {
        let connection = self.connect()?;
        connection
            .query_row(
                "SELECT job_id, job_type, status, stage, progress, processed_items, total_items, error_json, created_at, started_at, finished_at FROM jobs WHERE job_type = 'initial_scan' AND status IN ('queued', 'running', 'paused') ORDER BY created_at DESC LIMIT 1",
                [],
                job_from_row,
            )
            .optional()
            .map_err(|error| storage_error("JOB_QUERY_FAILED", error, true))
    }

    pub fn recover_interrupted_scan_jobs(&self) -> Result<Vec<(Uuid, JobRecord)>, AppError> {
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction()
            .map_err(|error| storage_error("DATABASE_TRANSACTION_FAILED", error, true))?;
        let interrupted = {
            let mut statement = transaction
                .prepare(
                    "SELECT job_id, root_id, resume_count FROM jobs WHERE job_type = 'initial_scan' AND status IN ('queued', 'running') AND root_id IS NOT NULL ORDER BY created_at",
                )
                .map_err(|error| storage_error("JOB_QUERY_FAILED", error, true))?;
            statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, u32>(2)?,
                    ))
                })
                .map_err(|error| storage_error("JOB_QUERY_FAILED", error, true))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| storage_error("JOB_QUERY_FAILED", error, true))?
        };
        let mut resumable = Vec::new();
        for (job_id, root_id, resume_count) in interrupted {
            if resume_count >= 3 {
                let error = AppError::new(
                    "JOB_RECOVERY_LIMIT_REACHED",
                    "扫描任务连续恢复三次仍未完成，已停止自动重试",
                    false,
                );
                transaction
                    .execute(
                        "UPDATE jobs SET status = 'failed', stage = 'recovery_failed', error_json = ?1, finished_at = ?2 WHERE job_id = ?3",
                        params![serde_json::to_string(&error).map_err(|serialize_error| AppError::new("JOB_ERROR_SERIALIZE_FAILED", serialize_error.to_string(), false))?, Utc::now().to_rfc3339(), job_id],
                    )
                    .map_err(|error| storage_error("JOB_UPDATE_FAILED", error, true))?;
                transaction
                    .execute(
                        "UPDATE roots SET status = 'failed' WHERE root_id = ?1",
                        [&root_id],
                    )
                    .map_err(|error| storage_error("ROOT_UPDATE_FAILED", error, true))?;
            } else {
                transaction
                    .execute(
                        "UPDATE jobs SET status = 'queued', stage = 'recovery_pending', started_at = NULL, finished_at = NULL, error_json = NULL, resume_count = resume_count + 1, last_heartbeat_at = ?1 WHERE job_id = ?2",
                        params![Utc::now().to_rfc3339(), job_id],
                    )
                    .map_err(|error| storage_error("JOB_UPDATE_FAILED", error, true))?;
                transaction
                    .execute(
                        "UPDATE roots SET status = 'paused' WHERE root_id = ?1",
                        [&root_id],
                    )
                    .map_err(|error| storage_error("ROOT_UPDATE_FAILED", error, true))?;
                resumable.push((parse_uuid_value(&root_id)?, parse_uuid_value(&job_id)?));
            }
        }
        transaction
            .commit()
            .map_err(|error| storage_error("DATABASE_COMMIT_FAILED", error, true))?;
        resumable
            .into_iter()
            .map(|(root_id, job_id)| self.job_by_id(&job_id).map(|job| (root_id, job)))
            .collect()
    }

    pub fn mark_scan_running(&self, root_id: &Uuid, job_id: &Uuid) -> Result<(), AppError> {
        let connection = self.connect()?;
        let transaction = connection
            .unchecked_transaction()
            .map_err(|error| storage_error("DATABASE_TRANSACTION_FAILED", error, true))?;
        transaction
            .execute(
                "UPDATE roots SET status = 'scanning' WHERE root_id = ?1",
                [root_id.to_string()],
            )
            .map_err(|error| storage_error("ROOT_UPDATE_FAILED", error, true))?;
        transaction
            .execute(
                "UPDATE jobs SET status = 'running', stage = 'enumerating', started_at = ?1, last_heartbeat_at = ?1 WHERE job_id = ?2",
                params![Utc::now().to_rfc3339(), job_id.to_string()],
            )
            .map_err(|error| storage_error("JOB_UPDATE_FAILED", error, true))?;
        transaction
            .commit()
            .map_err(|error| storage_error("DATABASE_COMMIT_FAILED", error, true))
    }

    pub fn pause_scan(&self, job_id: &Uuid) -> Result<JobRecord, AppError> {
        let root_id = self.root_id_for_job(job_id)?;
        let connection = self.connect()?;
        let transaction = connection
            .unchecked_transaction()
            .map_err(|error| storage_error("DATABASE_TRANSACTION_FAILED", error, true))?;
        let changed = transaction
            .execute(
                "UPDATE jobs SET status = 'paused', stage = 'paused', last_heartbeat_at = ?1 WHERE job_id = ?2 AND status = 'running'",
                params![Utc::now().to_rfc3339(), job_id.to_string()],
            )
            .map_err(|error| storage_error("JOB_UPDATE_FAILED", error, true))?;
        if changed == 0 {
            return Err(AppError::new(
                "JOB_TRANSITION_INVALID",
                "只有运行中的扫描任务可以暂停",
                false,
            ));
        }
        transaction
            .execute(
                "UPDATE roots SET status = 'paused' WHERE root_id = ?1",
                [root_id.to_string()],
            )
            .map_err(|error| storage_error("ROOT_UPDATE_FAILED", error, true))?;
        transaction
            .commit()
            .map_err(|error| storage_error("DATABASE_COMMIT_FAILED", error, true))?;
        self.job_by_id(job_id)
    }

    pub fn resume_scan(&self, job_id: &Uuid) -> Result<JobRecord, AppError> {
        let root_id = self.root_id_for_job(job_id)?;
        let connection = self.connect()?;
        let transaction = connection
            .unchecked_transaction()
            .map_err(|error| storage_error("DATABASE_TRANSACTION_FAILED", error, true))?;
        let changed = transaction
            .execute(
                "UPDATE jobs SET status = 'running', stage = 'enumerating', last_heartbeat_at = ?1 WHERE job_id = ?2 AND status = 'paused'",
                params![Utc::now().to_rfc3339(), job_id.to_string()],
            )
            .map_err(|error| storage_error("JOB_UPDATE_FAILED", error, true))?;
        if changed == 0 {
            return Err(AppError::new(
                "JOB_TRANSITION_INVALID",
                "只有已暂停的扫描任务可以继续",
                false,
            ));
        }
        transaction
            .execute(
                "UPDATE roots SET status = 'scanning' WHERE root_id = ?1",
                [root_id.to_string()],
            )
            .map_err(|error| storage_error("ROOT_UPDATE_FAILED", error, true))?;
        transaction
            .commit()
            .map_err(|error| storage_error("DATABASE_COMMIT_FAILED", error, true))?;
        self.job_by_id(job_id)
    }

    pub fn cancel_scan(&self, job_id: &Uuid) -> Result<JobRecord, AppError> {
        let root_id = self.root_id_for_job(job_id)?;
        let current = self.job_by_id(job_id)?;
        if current.status == JobStatus::Cancelled {
            return Ok(current);
        }
        let connection = self.connect()?;
        let transaction = connection
            .unchecked_transaction()
            .map_err(|error| storage_error("DATABASE_TRANSACTION_FAILED", error, true))?;
        let changed = transaction
            .execute(
                "UPDATE jobs SET status = 'cancelled', stage = 'cancelled', finished_at = ?1, last_heartbeat_at = ?1 WHERE job_id = ?2 AND status IN ('queued', 'running', 'paused', 'awaiting_user')",
                params![Utc::now().to_rfc3339(), job_id.to_string()],
            )
            .map_err(|error| storage_error("JOB_UPDATE_FAILED", error, true))?;
        if changed == 0 {
            return Err(AppError::new(
                "JOB_TRANSITION_INVALID",
                "已结束的扫描任务不能取消",
                false,
            ));
        }
        transaction
            .execute(
                "UPDATE roots SET status = 'ready' WHERE root_id = ?1",
                [root_id.to_string()],
            )
            .map_err(|error| storage_error("ROOT_UPDATE_FAILED", error, true))?;
        transaction
            .commit()
            .map_err(|error| storage_error("DATABASE_COMMIT_FAILED", error, true))?;
        self.job_by_id(job_id)
    }

    fn root_id_for_job(&self, job_id: &Uuid) -> Result<Uuid, AppError> {
        let connection = self.connect()?;
        let root_id: String = connection
            .query_row(
                "SELECT root_id FROM jobs WHERE job_id = ?1 AND root_id IS NOT NULL",
                [job_id.to_string()],
                |row| row.get(0),
            )
            .map_err(|error| storage_error("JOB_NOT_FOUND", error, false))?;
        parse_uuid_value(&root_id)
    }

    pub fn commit_scan(
        &self,
        root_id: &Uuid,
        job_id: &Uuid,
        outcome: &ScanOutcome,
    ) -> Result<JobRecord, AppError> {
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction()
            .map_err(|error| storage_error("DATABASE_TRANSACTION_FAILED", error, true))?;
        let mut seen_file_ids = Vec::with_capacity(outcome.files.len());
        for file in &outcome.files {
            seen_file_ids.push(upsert_file(&transaction, root_id, file)?);
        }
        if outcome.error_count == 0 && !outcome.deferred_by_budget {
            reconcile_root_memberships(&transaction, root_id, &seen_file_ids)?;
        }

        let status = if outcome.error_count > 0 || outcome.deferred_by_budget {
            JobStatus::Partial
        } else {
            JobStatus::Succeeded
        };
        let root_status = if outcome.error_count > 0 {
            RootStatus::PartialDenied
        } else {
            RootStatus::Ready
        };
        let finished_at = Utc::now();
        transaction
            .execute(
                "UPDATE roots SET status = ?1, file_count = ?2, error_count = ?3, permission_error_count = ?3, last_scanned_at = ?4, last_scan_at = ?4 WHERE root_id = ?5",
                params![
                    root_status.as_str(),
                    outcome.files.len() as u64,
                    outcome.error_count,
                    finished_at.to_rfc3339(),
                    root_id.to_string(),
                ],
            )
            .map_err(|error| storage_error("ROOT_UPDATE_FAILED", error, true))?;
        transaction
            .execute(
                "UPDATE jobs SET status = ?1, stage = 'completed', progress = 1.0, processed_items = ?2, total_items = ?2, finished_at = ?3 WHERE job_id = ?4",
                params![
                    status.as_str(),
                    outcome.files.len() as u64,
                    finished_at.to_rfc3339(),
                    job_id.to_string(),
                ],
            )
            .map_err(|error| storage_error("JOB_UPDATE_FAILED", error, true))?;
        transaction
            .commit()
            .map_err(|error| storage_error("DATABASE_COMMIT_FAILED", error, true))?;

        self.job_by_id(job_id)
    }

    pub fn fail_scan(
        &self,
        root_id: &Uuid,
        job_id: &Uuid,
        error: AppError,
    ) -> Result<JobRecord, AppError> {
        let connection = self.connect()?;
        let transaction = connection
            .unchecked_transaction()
            .map_err(|database_error| {
                storage_error("DATABASE_TRANSACTION_FAILED", database_error, true)
            })?;
        let finished_at = Utc::now().to_rfc3339();
        let error_json = serde_json::to_string(&error).map_err(|json_error| {
            AppError::new("JOB_ERROR_SERIALIZE_FAILED", json_error.to_string(), false)
        })?;
        transaction
            .execute(
                "UPDATE roots SET status = 'failed', error_count = error_count + 1 WHERE root_id = ?1",
                [root_id.to_string()],
            )
            .map_err(|database_error| storage_error("ROOT_UPDATE_FAILED", database_error, true))?;
        transaction
            .execute(
                "UPDATE jobs SET status = 'failed', stage = 'failed', error_json = ?1, finished_at = ?2 WHERE job_id = ?3",
                params![error_json, finished_at, job_id.to_string()],
            )
            .map_err(|database_error| storage_error("JOB_UPDATE_FAILED", database_error, true))?;
        transaction.commit().map_err(|database_error| {
            storage_error("DATABASE_COMMIT_FAILED", database_error, true)
        })?;
        self.job_by_id(job_id)
    }

    pub fn list_files(&self) -> Result<Vec<FileRecord>, AppError> {
        let connection = self.connect()?;
        let mut statement = connection
            .prepare(
                "SELECT file_id, volume_id, canonical_path, display_name, extension, mime_type, size_bytes, fs_created_at, modified_at, windows_file_id, content_sha256, availability, current_revision_id, parse_status, first_seen_at, last_seen_at FROM files ORDER BY last_seen_at DESC",
            )
            .map_err(|error| storage_error("FILE_QUERY_FAILED", error, true))?;
        statement
            .query_map([], file_from_row)
            .map_err(|error| storage_error("FILE_QUERY_FAILED", error, true))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| storage_error("FILE_QUERY_FAILED", error, true))
    }

    pub fn query_files(&self, request: &FileQuery) -> Result<FilePage, AppError> {
        let connection = self.connect()?;
        let offset = request.offset()?;
        let page_size = u64::from(request.validated_page_size());
        let total = connection
            .query_row("SELECT COUNT(*) FROM files", [], |row| row.get::<_, u64>(0))
            .map_err(|error| storage_error("FILE_QUERY_FAILED", error, true))?;
        let mut statement = connection
            .prepare(
                "SELECT file_id, volume_id, canonical_path, display_name, extension, mime_type, size_bytes, fs_created_at, modified_at, windows_file_id, content_sha256, availability, current_revision_id, parse_status, first_seen_at, last_seen_at FROM files ORDER BY last_seen_at DESC, file_id DESC LIMIT ?1 OFFSET ?2",
            )
            .map_err(|error| storage_error("FILE_QUERY_FAILED", error, true))?;
        let items = statement
            .query_map(params![page_size, offset], file_from_row)
            .map_err(|error| storage_error("FILE_QUERY_FAILED", error, true))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| storage_error("FILE_QUERY_FAILED", error, true))?;
        let consumed = offset.saturating_add(items.len() as u64);
        Ok(FilePage {
            items,
            next_cursor: (consumed < total).then(|| consumed.to_string()),
            total,
        })
    }

    pub fn home_file_summary(&self, local_date: &str) -> Result<(u64, Vec<FileRecord>), AppError> {
        let connection = self.connect()?;
        let today_added = connection
            .query_row(
                "SELECT COUNT(*) FROM files WHERE substr(first_seen_at, 1, 10) = ?1",
                [local_date],
                |row| row.get::<_, u64>(0),
            )
            .map_err(|error| storage_error("FILE_QUERY_FAILED", error, true))?;
        let mut statement = connection
            .prepare(
                "SELECT file_id, volume_id, canonical_path, display_name, extension, mime_type, size_bytes, fs_created_at, modified_at, windows_file_id, content_sha256, availability, current_revision_id, parse_status, first_seen_at, last_seen_at FROM files ORDER BY last_seen_at DESC, file_id DESC LIMIT 8",
            )
            .map_err(|error| storage_error("FILE_QUERY_FAILED", error, true))?;
        let recent = statement
            .query_map([], file_from_row)
            .map_err(|error| storage_error("FILE_QUERY_FAILED", error, true))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| storage_error("FILE_QUERY_FAILED", error, true))?;
        Ok((today_added, recent))
    }

    pub fn run_extraction(
        &self,
        request: &ExtractionRunRequest,
    ) -> Result<ExtractionRunResult, AppError> {
        request.validate()?;
        let connection = self.connect()?;
        let mut documents = Vec::with_capacity(request.file_ids.len());
        for file_id in &request.file_ids {
            let file = authorized_file_by_id(&connection, file_id)?;
            let revision_id = file.current_revision_id.ok_or_else(|| {
                AppError::new(
                    "EXTRACTION_FILE_NOT_INDEXED",
                    format!("{}尚未建立正文索引", file.display_name),
                    true,
                )
            })?;
            let mut statement = connection
                .prepare("SELECT c.node_id, c.chunk_id, c.text, c.locator_json, n.node_type FROM chunks c JOIN document_nodes n ON n.node_id = c.node_id WHERE c.file_id = ?1 AND c.revision_id = ?2 ORDER BY c.ordinal LIMIT 5000")
                .map_err(|error| storage_error("EXTRACTION_QUERY_FAILED", error, true))?;
            let chunks = statement
                .query_map(
                    params![file_id.to_string(), revision_id.to_string()],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, String>(4)?,
                        ))
                    },
                )
                .map_err(|error| storage_error("EXTRACTION_QUERY_FAILED", error, true))?
                .map(|row| {
                    let (node_id, chunk_id, text, locator_json, node_type) =
                        row.map_err(|error| storage_error("EXTRACTION_QUERY_FAILED", error, true))?;
                    Ok(ExtractionChunk {
                        node_id: parse_uuid_value(&node_id)?,
                        chunk_id: parse_uuid_value(&chunk_id)?,
                        node_type,
                        text,
                        locator: serde_json::from_str(&locator_json).map_err(|error| {
                            AppError::new("EXTRACTION_EVIDENCE_INVALID", error.to_string(), false)
                        })?,
                    })
                })
                .collect::<Result<Vec<_>, AppError>>()?;
            let mut table_statement = connection
                .prepare("SELECT node_id, ordinal, table_json, locator_json FROM document_nodes WHERE revision_id = ?1 AND table_json IS NOT NULL ORDER BY ordinal LIMIT 50000")
                .map_err(|error| storage_error("EXTRACTION_QUERY_FAILED", error, true))?;
            let tables = table_statement
                .query_map([revision_id.to_string()], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, u32>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                })
                .map_err(|error| storage_error("EXTRACTION_QUERY_FAILED", error, true))?
                .map(|row| {
                    let (node_id, ordinal, table_json, locator_json) =
                        row.map_err(|error| storage_error("EXTRACTION_QUERY_FAILED", error, true))?;
                    Ok(ExtractionTable {
                        node_id: parse_uuid_value(&node_id)?,
                        ordinal,
                        table_data: serde_json::from_str(&table_json).map_err(|error| {
                            AppError::new("EXTRACTION_TABLE_INVALID", error.to_string(), false)
                        })?,
                        locator: serde_json::from_str(&locator_json).map_err(|error| {
                            AppError::new("EXTRACTION_EVIDENCE_INVALID", error.to_string(), false)
                        })?,
                    })
                })
                .collect::<Result<Vec<_>, AppError>>()?;
            documents.push(ExtractionDocument {
                file,
                revision_id,
                chunks,
                tables,
            });
        }
        crate::run_rules_first_extraction(request, documents)
    }

    pub fn query_inbox(&self, query: &InboxQuery) -> Result<InboxPage, AppError> {
        query.validate()?;
        let connection = self.connect()?;
        let collections = collection_rows(&connection)?;
        let mut statement = connection
            .prepare(
                "SELECT f.file_id, f.volume_id, f.canonical_path, f.display_name, f.extension, f.mime_type, f.size_bytes, f.fs_created_at, f.modified_at, f.windows_file_id, f.content_sha256, f.availability, f.current_revision_id, f.parse_status, f.first_seen_at, f.last_seen_at, i.inbox_id, i.event_type, i.observed_at, i.previous_path, i.triage_status, i.summary, i.error_code, (SELECT relation_id FROM file_relations r WHERE r.relation_type = 'exact_duplicate' AND (r.left_file_id = f.file_id OR r.right_file_id = f.file_id) AND r.review_status <> 'rejected' ORDER BY r.confidence DESC LIMIT 1) FROM inbox_events i JOIN files f ON f.file_id = i.file_id WHERE EXISTS (SELECT 1 FROM file_root_memberships m JOIN roots rt ON rt.root_id = m.root_id WHERE m.file_id = f.file_id AND rt.enabled = 1) ORDER BY i.observed_at DESC",
            )
            .map_err(|error| storage_error("INBOX_QUERY_FAILED", error, true))?;
        let rows = statement
            .query_map([], |row| {
                let file = file_from_row(row)?;
                let inbox_id: String = row.get(16)?;
                let event_type: String = row.get(17)?;
                let observed_at: String = row.get(18)?;
                let triage_status: String = row.get(20)?;
                let duplicate_group_id: Option<String> = row.get(23)?;
                Ok((
                    file,
                    parse_uuid_column(&inbox_id, 16)?,
                    InboxEventType::from_storage(&event_type),
                    parse_datetime_column(&observed_at, 18)?,
                    row.get::<_, Option<String>>(19)?,
                    TriageStatus::from_storage(&triage_status),
                    row.get::<_, Option<String>>(21)?,
                    row.get::<_, Option<String>>(22)?,
                    duplicate_group_id
                        .map(|value| parse_uuid_column(&value, 23))
                        .transpose()?,
                ))
            })
            .map_err(|error| storage_error("INBOX_QUERY_FAILED", error, true))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| storage_error("INBOX_QUERY_FAILED", error, true))?;

        let cursor_time = query
            .cursor
            .as_deref()
            .map(DateTime::parse_from_rfc3339)
            .transpose()
            .map_err(|error| AppError::new("INBOX_CURSOR_INVALID", error.to_string(), false))?
            .map(|value| value.with_timezone(&Utc));
        let mut items = Vec::new();
        for (
            file,
            inbox_id,
            event_type,
            observed_at,
            previous_path,
            triage_status,
            summary,
            error_code,
            duplicate_group_id,
        ) in rows
        {
            if query.status != TriageStatus::All && query.status != triage_status {
                continue;
            }
            if !query.event_types.is_empty() && !query.event_types.contains(&event_type) {
                continue;
            }
            if query.date_from.is_some_and(|from| observed_at < from)
                || query.date_to.is_some_and(|to| observed_at > to)
                || cursor_time.is_some_and(|cursor| observed_at >= cursor)
            {
                continue;
            }
            if !query.root_ids.is_empty()
                && !file_belongs_to_any_root(&connection, &file.file_id, &query.root_ids)?
            {
                continue;
            }
            let mut suggested_collection_ids = Vec::new();
            for collection in &collections {
                if let Some(rule) = &collection.rule
                    && collection_rule_matches(&connection, rule, &file)?
                {
                    suggested_collection_ids.push(collection.collection_id);
                }
            }
            items.push(InboxItem {
                inbox_id,
                file_id: file.file_id,
                display_name: file.display_name,
                canonical_path: file.canonical_path,
                event_type,
                observed_at,
                previous_path,
                triage_status,
                suggested_collection_ids,
                duplicate_group_id,
                summary,
                error_code,
            });
            if items.len() > query.page_size as usize {
                break;
            }
        }
        let next_cursor = if items.len() > query.page_size as usize {
            items.truncate(query.page_size as usize);
            items.last().map(|item| item.observed_at.to_rfc3339())
        } else {
            None
        };
        Ok(InboxPage { items, next_cursor })
    }

    pub fn update_inbox_item(&self, request: &InboxUpdateRequest) -> Result<InboxItem, AppError> {
        if matches!(
            request.triage_status,
            TriageStatus::All | TriageStatus::Error
        ) {
            return Err(AppError::new(
                "INBOX_UPDATE_INVALID",
                "收件箱人工操作只能标为已查看、忽略或待处理",
                false,
            ));
        }
        let connection = self.connect()?;
        let changed = connection
            .execute(
                "UPDATE inbox_events SET triage_status = ?1, processed_at = CASE WHEN ?1 = 'new' THEN NULL ELSE ?2 END WHERE inbox_id = ?3",
                params![request.triage_status.as_storage(), Utc::now().to_rfc3339(), request.inbox_id.to_string()],
            )
            .map_err(|error| storage_error("INBOX_UPDATE_FAILED", error, true))?;
        if changed == 0 {
            return Err(AppError::new(
                "INBOX_ITEM_NOT_FOUND",
                "收件箱项目不存在",
                false,
            ));
        }
        let query = InboxQuery {
            status: TriageStatus::All,
            event_types: vec![],
            root_ids: vec![],
            date_from: None,
            date_to: None,
            cursor: None,
            page_size: 200,
        };
        self.query_inbox(&query)?
            .items
            .into_iter()
            .find(|item| item.inbox_id == request.inbox_id)
            .ok_or_else(|| AppError::new("INBOX_ITEM_NOT_FOUND", "收件箱项目不可访问", false))
    }

    pub fn list_collections(&self) -> Result<Vec<CollectionRecord>, AppError> {
        let connection = self.connect()?;
        collection_rows(&connection)
    }

    pub fn create_collection(
        &self,
        request: &CreateCollectionRequest,
    ) -> Result<CollectionRecord, AppError> {
        request.validate()?;
        let connection = self.connect()?;
        let collection_id = Uuid::now_v7();
        let now = Utc::now().to_rfc3339();
        let rule_json = request
            .rule
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|error| AppError::new("COLLECTION_RULE_INVALID", error.to_string(), false))?;
        connection
            .execute(
                "INSERT INTO collections (collection_id, name, description, icon, color, kind, rule_json, built_in, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, ?8, ?8)",
                params![collection_id.to_string(), request.name.trim(), request.description, request.icon, request.color, request.kind.as_storage(), rule_json, now],
            )
            .map_err(|error| storage_error("COLLECTION_CREATE_FAILED", error, false))?;
        collection_by_id(&connection, &collection_id)
    }

    pub fn update_collection(
        &self,
        collection_id: &Uuid,
        request: &CreateCollectionRequest,
    ) -> Result<CollectionRecord, AppError> {
        request.validate()?;
        let connection = self.connect()?;
        let current = collection_by_id(&connection, collection_id)?;
        if current.built_in {
            return Err(AppError::new(
                "COLLECTION_BUILT_IN_READONLY",
                "内置集合不能修改",
                false,
            ));
        }
        let rule_json = request
            .rule
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|error| AppError::new("COLLECTION_RULE_INVALID", error.to_string(), false))?;
        connection
            .execute(
                "UPDATE collections SET name = ?1, description = ?2, icon = ?3, color = ?4, kind = ?5, rule_json = ?6, updated_at = ?7 WHERE collection_id = ?8 AND built_in = 0",
                params![request.name.trim(), request.description, request.icon, request.color, request.kind.as_storage(), rule_json, Utc::now().to_rfc3339(), collection_id.to_string()],
            )
            .map_err(|error| storage_error("COLLECTION_UPDATE_FAILED", error, true))?;
        collection_by_id(&connection, collection_id)
    }

    pub fn delete_collection(&self, collection_id: &Uuid) -> Result<(), AppError> {
        let mut connection = self.connect()?;
        let current = collection_by_id(&connection, collection_id)?;
        if current.built_in {
            return Err(AppError::new(
                "COLLECTION_BUILT_IN_READONLY",
                "内置集合不能删除",
                false,
            ));
        }
        let transaction = connection
            .transaction()
            .map_err(|error| storage_error("DATABASE_TRANSACTION_FAILED", error, true))?;
        transaction
            .execute(
                "DELETE FROM collection_memberships WHERE collection_id = ?1",
                [collection_id.to_string()],
            )
            .map_err(|error| storage_error("COLLECTION_DELETE_FAILED", error, true))?;
        transaction
            .execute(
                "DELETE FROM collections WHERE collection_id = ?1 AND built_in = 0",
                [collection_id.to_string()],
            )
            .map_err(|error| storage_error("COLLECTION_DELETE_FAILED", error, true))?;
        transaction
            .commit()
            .map_err(|error| storage_error("DATABASE_COMMIT_FAILED", error, true))
    }

    pub fn add_file_to_collection(
        &self,
        collection_id: &Uuid,
        file_id: &Uuid,
    ) -> Result<(), AppError> {
        let connection = self.connect()?;
        let collection = collection_by_id(&connection, collection_id)?;
        if collection.kind != CollectionKind::Manual {
            return Err(AppError::new(
                "COLLECTION_MEMBERSHIP_INVALID",
                "规则集合由规则自动维护，不能手动添加资料",
                false,
            ));
        }
        authorized_file_by_id(&connection, file_id)?;
        connection
            .execute(
                "INSERT OR IGNORE INTO collection_memberships (collection_id, file_id, source, created_at) VALUES (?1, ?2, 'manual', ?3)",
                params![collection_id.to_string(), file_id.to_string(), Utc::now().to_rfc3339()],
            )
            .map_err(|error| storage_error("COLLECTION_MEMBERSHIP_FAILED", error, true))?;
        Ok(())
    }

    pub fn remove_file_from_collection(
        &self,
        collection_id: &Uuid,
        file_id: &Uuid,
    ) -> Result<(), AppError> {
        let connection = self.connect()?;
        let collection = collection_by_id(&connection, collection_id)?;
        if collection.kind != CollectionKind::Manual {
            return Err(AppError::new(
                "COLLECTION_MEMBERSHIP_INVALID",
                "规则集合由规则自动维护，不能手动移除资料",
                false,
            ));
        }
        connection
            .execute(
                "DELETE FROM collection_memberships WHERE collection_id = ?1 AND file_id = ?2",
                params![collection_id.to_string(), file_id.to_string()],
            )
            .map_err(|error| storage_error("COLLECTION_MEMBERSHIP_FAILED", error, true))?;
        Ok(())
    }

    pub fn preview_collection_rule(
        &self,
        rule: &CollectionRule,
        limit: u32,
    ) -> Result<Vec<FileRecord>, AppError> {
        rule.validate()?;
        let connection = self.connect()?;
        query_files_for_rule(&connection, rule, 0, limit.clamp(1, 100))
    }

    pub fn collection_files(&self, collection_id: &Uuid) -> Result<Vec<FileRecord>, AppError> {
        let connection = self.connect()?;
        let collection = collection_by_id(&connection, collection_id)?;
        let files = list_files_with_connection(&connection)?;
        match collection.kind {
            CollectionKind::Manual => Ok(files
                .into_iter()
                .filter(|file| {
                    connection
                        .query_row(
                            "SELECT EXISTS(SELECT 1 FROM collection_memberships WHERE collection_id = ?1 AND file_id = ?2)",
                            params![collection_id.to_string(), file.file_id.to_string()],
                            |row| row.get::<_, i64>(0),
                        )
                        .unwrap_or_default()
                        != 0
                })
                .collect()),
            CollectionKind::Rule => {
                let rule = collection.rule.ok_or_else(|| {
                    AppError::new("COLLECTION_RULE_INVALID", "规则集合缺少规则", false)
                })?;
                let mut matched = Vec::new();
                for file in files {
                    if file_is_authorized(&connection, &file.file_id)?
                        && collection_rule_matches(&connection, &rule, &file)?
                    {
                        matched.push(file);
                    }
                }
                Ok(matched)
            }
        }
    }

    pub fn query_collection_files(
        &self,
        collection_id: &Uuid,
        request: &FileQuery,
    ) -> Result<FilePage, AppError> {
        let connection = self.connect()?;
        let collection = collection_by_id(&connection, collection_id)?;
        let offset = request.offset()?;
        let page_size = request.validated_page_size();
        let (total, items) = match collection.kind {
            CollectionKind::Manual => (
                count_manual_collection_files(&connection, collection_id)?,
                query_manual_collection_files(&connection, collection_id, offset, page_size)?,
            ),
            CollectionKind::Rule => {
                let rule = collection.rule.ok_or_else(|| {
                    AppError::new("COLLECTION_RULE_INVALID", "规则集合缺少规则", false)
                })?;
                (
                    count_files_for_rule(&connection, &rule)?,
                    query_files_for_rule(&connection, &rule, offset, page_size)?,
                )
            }
        };
        let consumed = offset.saturating_add(items.len() as u64);
        Ok(FilePage {
            items,
            next_cursor: (consumed < total).then(|| consumed.to_string()),
            total,
        })
    }

    pub fn refresh_file_relations(
        &self,
        max_files: u32,
    ) -> Result<RelationRefreshResult, AppError> {
        if !(2..=20_000).contains(&max_files) {
            return Err(AppError::new(
                "RELATION_REFRESH_INVALID",
                "关系刷新文件预算必须在2到20000之间",
                false,
            ));
        }
        let mut connection = self.connect()?;
        let files = list_files_with_connection(&connection)?
            .into_iter()
            .filter(|file| {
                file.availability == crate::Availability::Present
                    && file.size_bytes > 0
                    && file_is_authorized(&connection, &file.file_id).unwrap_or(false)
            })
            .take(max_files as usize)
            .collect::<Vec<_>>();
        let mut size_counts = BTreeMap::<u64, usize>::new();
        for file in &files {
            *size_counts.entry(file.size_bytes).or_default() += 1;
        }

        let mut hashed_files = 0_u64;
        let mut hashes = BTreeMap::<String, Vec<Uuid>>::new();
        let transaction = connection
            .transaction()
            .map_err(|error| storage_error("DATABASE_TRANSACTION_FAILED", error, true))?;
        for file in &files {
            if size_counts
                .get(&file.size_bytes)
                .copied()
                .unwrap_or_default()
                < 2
            {
                continue;
            }
            let hash = if let Some(hash) = &file.content_sha256 {
                hash.clone()
            } else {
                let hash = hash_file_sha256(&PathBuf::from(&file.canonical_path))?;
                transaction
                    .execute(
                        "UPDATE files SET content_sha256 = ?1 WHERE file_id = ?2 AND current_revision_id = ?3",
                        params![hash, file.file_id.to_string(), file.current_revision_id.map(|value| value.to_string())],
                    )
                    .map_err(|error| storage_error("FILE_HASH_UPDATE_FAILED", error, true))?;
                if let Some(revision_id) = file.current_revision_id {
                    transaction
                        .execute(
                            "UPDATE file_revisions SET content_sha256 = ?1 WHERE revision_id = ?2",
                            params![hash, revision_id.to_string()],
                        )
                        .map_err(|error| storage_error("FILE_HASH_UPDATE_FAILED", error, true))?;
                }
                hashed_files += 1;
                hash
            };
            hashes.entry(hash).or_default().push(file.file_id);
        }
        transaction
            .execute(
                "DELETE FROM file_relations WHERE relation_type IN ('exact_duplicate', 'version_candidate') AND review_status = 'suggested'",
                [],
            )
            .map_err(|error| storage_error("RELATION_REFRESH_FAILED", error, true))?;

        let mut exact_duplicate_pairs = 0_u64;
        for group in hashes.values().filter(|group| group.len() > 1) {
            for left_index in 0..group.len() {
                for right_index in (left_index + 1)..group.len() {
                    insert_relation(
                        &transaction,
                        group[left_index],
                        group[right_index],
                        RelationType::ExactDuplicate,
                        1.0,
                        &["文件字节SHA-256完全相同".to_owned()],
                    )?;
                    exact_duplicate_pairs += 1;
                }
            }
        }

        let mut version_groups = BTreeMap::<(String, String), Vec<Uuid>>::new();
        for file in &files {
            let key = normalized_version_key(&file.display_name);
            if !key.is_empty() {
                version_groups
                    .entry((key, file.extension.to_lowercase()))
                    .or_default()
                    .push(file.file_id);
            }
        }
        let mut version_candidate_pairs = 0_u64;
        for group in version_groups.values().filter(|group| group.len() > 1) {
            for left_index in 0..group.len() {
                for right_index in (left_index + 1)..group.len() {
                    insert_relation(
                        &transaction,
                        group[left_index],
                        group[right_index],
                        RelationType::VersionCandidate,
                        0.78,
                        &["文件名去除版本与副本后缀后相同".to_owned()],
                    )?;
                    version_candidate_pairs += 1;
                }
            }
        }
        transaction
            .commit()
            .map_err(|error| storage_error("DATABASE_COMMIT_FAILED", error, true))?;
        Ok(RelationRefreshResult {
            hashed_files,
            exact_duplicate_pairs,
            version_candidate_pairs,
        })
    }

    pub fn refresh_selected_file_relations(
        &self,
        file_ids: &[Uuid],
    ) -> Result<RelationRefreshResult, AppError> {
        if !(2..=500).contains(&file_ids.len()) {
            return Err(AppError::new(
                "RELATION_SELECTION_INVALID",
                "重复审查需要选择2到500份资料",
                false,
            ));
        }
        let mut connection = self.connect()?;
        let files = file_ids
            .iter()
            .map(|file_id| authorized_file_by_id(&connection, file_id))
            .collect::<Result<Vec<_>, _>>()?;
        let mut size_counts = BTreeMap::<u64, usize>::new();
        for file in &files {
            *size_counts.entry(file.size_bytes).or_default() += 1;
        }
        let transaction = connection
            .transaction()
            .map_err(|error| storage_error("DATABASE_TRANSACTION_FAILED", error, true))?;
        let mut hashed_files = 0_u64;
        let mut hashes = BTreeMap::<String, Vec<Uuid>>::new();
        for file in &files {
            if file.size_bytes == 0 || size_counts.get(&file.size_bytes).copied().unwrap_or(0) < 2 {
                continue;
            }
            let hash = if let Some(hash) = &file.content_sha256 {
                hash.clone()
            } else {
                let hash = hash_file_sha256(&PathBuf::from(&file.canonical_path))?;
                transaction
                    .execute(
                        "UPDATE files SET content_sha256 = ?1 WHERE file_id = ?2 AND current_revision_id = ?3",
                        params![hash, file.file_id.to_string(), file.current_revision_id.map(|value| value.to_string())],
                    )
                    .map_err(|error| storage_error("FILE_HASH_UPDATE_FAILED", error, true))?;
                if let Some(revision_id) = file.current_revision_id {
                    transaction
                        .execute(
                            "UPDATE file_revisions SET content_sha256 = ?1 WHERE revision_id = ?2",
                            params![hash, revision_id.to_string()],
                        )
                        .map_err(|error| storage_error("FILE_HASH_UPDATE_FAILED", error, true))?;
                }
                hashed_files += 1;
                hash
            };
            hashes.entry(hash).or_default().push(file.file_id);
        }
        let mut exact_duplicate_pairs = 0_u64;
        for group in hashes.values().filter(|group| group.len() > 1) {
            for left_index in 0..group.len() {
                for right_index in (left_index + 1)..group.len() {
                    insert_relation(
                        &transaction,
                        group[left_index],
                        group[right_index],
                        RelationType::ExactDuplicate,
                        1.0,
                        &["文件字节SHA-256完全相同".to_owned()],
                    )?;
                    exact_duplicate_pairs += 1;
                }
            }
        }
        let mut version_groups = BTreeMap::<(String, String), Vec<Uuid>>::new();
        for file in &files {
            let key = normalized_version_key(&file.display_name);
            if !key.is_empty() {
                version_groups
                    .entry((key, file.extension.to_lowercase()))
                    .or_default()
                    .push(file.file_id);
            }
        }
        let mut version_candidate_pairs = 0_u64;
        for group in version_groups.values().filter(|group| group.len() > 1) {
            for left_index in 0..group.len() {
                for right_index in (left_index + 1)..group.len() {
                    insert_relation(
                        &transaction,
                        group[left_index],
                        group[right_index],
                        RelationType::VersionCandidate,
                        0.78,
                        &["文件名去除版本与副本后缀后相同".to_owned()],
                    )?;
                    version_candidate_pairs += 1;
                }
            }
        }
        transaction
            .commit()
            .map_err(|error| storage_error("DATABASE_COMMIT_FAILED", error, true))?;
        Ok(RelationRefreshResult {
            hashed_files,
            exact_duplicate_pairs,
            version_candidate_pairs,
        })
    }

    pub fn authorized_files_by_ids(&self, file_ids: &[Uuid]) -> Result<Vec<FileRecord>, AppError> {
        let connection = self.connect()?;
        file_ids
            .iter()
            .map(|file_id| authorized_file_by_id(&connection, file_id))
            .collect()
    }

    pub fn list_file_relations(&self, limit: u32) -> Result<Vec<FileRelation>, AppError> {
        self.query_file_relations(&RelationQuery {
            cursor: None,
            page_size: limit,
        })
        .map(|page| page.items)
    }

    pub fn query_file_relations(&self, request: &RelationQuery) -> Result<RelationPage, AppError> {
        request.validate()?;
        let offset = request.offset()?;
        let page_size = u64::from(request.page_size);
        let connection = self.connect()?;
        let predicate = "review_status <> 'rejected' AND EXISTS (SELECT 1 FROM file_root_memberships m JOIN roots rt ON rt.root_id = m.root_id WHERE m.file_id = r.left_file_id AND rt.enabled = 1) AND EXISTS (SELECT 1 FROM file_root_memberships m JOIN roots rt ON rt.root_id = m.root_id WHERE m.file_id = r.right_file_id AND rt.enabled = 1)";
        let total = connection
            .query_row(
                &format!("SELECT COUNT(*) FROM file_relations r WHERE {predicate}"),
                [],
                |row| row.get::<_, u64>(0),
            )
            .map_err(|error| storage_error("RELATION_QUERY_FAILED", error, true))?;
        let raw = {
            let mut statement = connection
                .prepare(&format!(
                    "SELECT relation_id, relation_type, left_file_id, right_file_id, confidence, reasons_json, review_status, created_at FROM file_relations r WHERE {predicate} ORDER BY confidence DESC, created_at DESC, relation_id DESC LIMIT ?1 OFFSET ?2"
                ))
                .map_err(|error| storage_error("RELATION_QUERY_FAILED", error, true))?;
            statement
                .query_map(params![page_size, offset], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, f64>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, String>(7)?,
                    ))
                })
                .map_err(|error| storage_error("RELATION_QUERY_FAILED", error, true))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| storage_error("RELATION_QUERY_FAILED", error, true))?
        };
        let mut relations = Vec::new();
        for (
            relation_id,
            relation_type,
            left_file_id,
            right_file_id,
            confidence,
            reasons_json,
            review_status,
            created_at,
        ) in raw
        {
            let left_file_id = parse_uuid_value(&left_file_id)?;
            let right_file_id = parse_uuid_value(&right_file_id)?;
            let left_file = authorized_file_by_id(&connection, &left_file_id)?;
            let right_file = authorized_file_by_id(&connection, &right_file_id)?;
            relations.push(FileRelation {
                relation_id: parse_uuid_value(&relation_id)?,
                relation_type: RelationType::from_storage(&relation_type),
                left_file,
                right_file,
                confidence,
                reasons: serde_json::from_str(&reasons_json).map_err(|error| {
                    AppError::new("RELATION_DATA_INVALID", error.to_string(), false)
                })?,
                review_status,
                created_at: DateTime::parse_from_rfc3339(&created_at)
                    .map_err(|error| {
                        AppError::new("RELATION_DATA_INVALID", error.to_string(), false)
                    })?
                    .with_timezone(&Utc),
            });
        }
        let consumed = offset.saturating_add(relations.len() as u64);
        Ok(RelationPage {
            items: relations,
            next_cursor: (consumed < total).then(|| consumed.to_string()),
            total,
        })
    }

    pub fn review_file_relation(&self, relation_id: &Uuid, action: &str) -> Result<(), AppError> {
        if !matches!(action, "accepted" | "rejected") {
            return Err(AppError::new(
                "RELATION_REVIEW_INVALID",
                "关系复核动作只能是 accepted 或 rejected",
                false,
            ));
        }
        let connection = self.connect()?;
        let changed = connection
            .execute(
                "UPDATE file_relations SET review_status = ?1, updated_at = ?2 WHERE relation_id = ?3",
                params![action, Utc::now().to_rfc3339(), relation_id.to_string()],
            )
            .map_err(|error| storage_error("RELATION_REVIEW_FAILED", error, true))?;
        if changed == 0 {
            return Err(AppError::new(
                "RELATION_REVIEW_INVALID",
                "待复核的文件关系不存在",
                false,
            ));
        }
        Ok(())
    }

    pub fn list_pending_parse_files(&self, limit: usize) -> Result<Vec<FileRecord>, AppError> {
        let connection = self.connect()?;
        let mut statement = connection
            .prepare(
                "SELECT file_id, volume_id, canonical_path, display_name, extension, mime_type, size_bytes, fs_created_at, modified_at, windows_file_id, content_sha256, availability, current_revision_id, parse_status, first_seen_at, last_seen_at FROM files WHERE availability = 'present' AND current_revision_id IS NOT NULL AND parse_status = 'pending' AND extension IN ('pdf', 'docx', 'docm', 'xlsx', 'xlsm', 'pptx', 'pptm', 'csv', 'tsv', 'md', 'txt', 'html', 'htm', 'jpg', 'jpeg', 'png', 'tif', 'tiff', 'bmp', 'webp', 'doc', 'xls', 'ppt') ORDER BY last_seen_at LIMIT ?1",
            )
            .map_err(|error| storage_error("FILE_QUERY_FAILED", error, true))?;
        statement
            .query_map([limit as u64], file_from_row)
            .map_err(|error| storage_error("FILE_QUERY_FAILED", error, true))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| storage_error("FILE_QUERY_FAILED", error, true))
    }

    pub fn retry_ocr(&self, file_id: &Uuid) -> Result<(), AppError> {
        let connection = self.connect()?;
        let changed = connection
            .execute(
                "UPDATE files SET parse_status = 'pending' WHERE file_id = ?1 AND availability = 'present' AND current_revision_id IS NOT NULL AND parse_status = 'ocr_pending' AND EXISTS (SELECT 1 FROM file_root_memberships frm JOIN roots r ON r.root_id = frm.root_id WHERE frm.file_id = files.file_id AND r.enabled = 1)",
                [file_id.to_string()],
            )
            .map_err(|error| storage_error("OCR_RETRY_UPDATE_FAILED", error, true))?;
        if changed == 0 {
            return Err(AppError::new(
                "OCR_RETRY_NOT_AVAILABLE",
                "该资料当前不处于可重试的OCR状态",
                false,
            ));
        }
        Ok(())
    }

    pub fn list_pending_embedding_chunks(
        &self,
        model_artifact_id: &str,
        limit: usize,
    ) -> Result<Vec<PendingEmbeddingChunk>, AppError> {
        if model_artifact_id.trim().is_empty() || !(1..=512).contains(&limit) {
            return Err(AppError::new(
                "EMBEDDING_QUEUE_INVALID",
                "向量队列需要有效模型标识，批量大小为1到512",
                false,
            ));
        }
        let connection = self.connect()?;
        let mut statement = connection
            .prepare(
                "SELECT c.chunk_id, c.file_id, c.revision_id, c.text FROM chunks c JOIN files f ON f.file_id = c.file_id LEFT JOIN chunk_embeddings e ON e.chunk_id = c.chunk_id AND e.model_artifact_id = ?1 WHERE f.current_revision_id = c.revision_id AND f.availability = 'present' AND e.chunk_id IS NULL AND EXISTS (SELECT 1 FROM file_root_memberships m JOIN roots r ON r.root_id = m.root_id WHERE m.file_id = f.file_id AND r.enabled = 1) ORDER BY c.ordinal LIMIT ?2",
            )
            .map_err(|error| storage_error("EMBEDDING_QUEUE_QUERY_FAILED", error, true))?;
        statement
            .query_map(params![model_artifact_id, limit as u64], |row| {
                let chunk_id: String = row.get(0)?;
                let file_id: String = row.get(1)?;
                let revision_id: String = row.get(2)?;
                Ok(PendingEmbeddingChunk {
                    chunk_id: parse_uuid_column(&chunk_id, 0)?,
                    file_id: parse_uuid_column(&file_id, 1)?,
                    revision_id: parse_uuid_column(&revision_id, 2)?,
                    text: row.get(3)?,
                })
            })
            .map_err(|error| storage_error("EMBEDDING_QUEUE_QUERY_FAILED", error, true))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| storage_error("EMBEDDING_QUEUE_QUERY_FAILED", error, true))
    }

    pub fn commit_chunk_embeddings(
        &self,
        model_artifact_id: &str,
        dimension: u32,
        embeddings: &[ChunkEmbeddingInput],
    ) -> Result<u64, AppError> {
        if model_artifact_id.trim().is_empty()
            || dimension == 0
            || embeddings.is_empty()
            || embeddings.len() > 512
            || embeddings.iter().any(|embedding| {
                embedding.vector.len() != dimension as usize
                    || embedding.vector.iter().any(|value| !value.is_finite())
            })
        {
            return Err(AppError::new(
                "EMBEDDING_COMMIT_INVALID",
                "向量批次维度、数量或数值无效",
                false,
            ));
        }
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction()
            .map_err(|error| storage_error("DATABASE_TRANSACTION_FAILED", error, true))?;
        let mut committed = 0_u64;
        for embedding in embeddings {
            let metadata: Option<(String, String)> = transaction
                .query_row(
                    "SELECT c.file_id, c.revision_id FROM chunks c JOIN files f ON f.file_id = c.file_id WHERE c.chunk_id = ?1 AND f.current_revision_id = c.revision_id",
                    [embedding.chunk_id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(|error| storage_error("EMBEDDING_QUEUE_QUERY_FAILED", error, true))?;
            let Some((file_id, revision_id)) = metadata else {
                continue;
            };
            let vector_blob = encode_vector(&embedding.vector);
            transaction
                .execute(
                    "INSERT INTO chunk_embeddings (chunk_id, model_artifact_id, file_id, revision_id, dimension, vector_blob, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) ON CONFLICT(chunk_id, model_artifact_id) DO UPDATE SET dimension = excluded.dimension, vector_blob = excluded.vector_blob, created_at = excluded.created_at",
                    params![embedding.chunk_id.to_string(), model_artifact_id, file_id, revision_id, dimension, vector_blob, Utc::now().to_rfc3339()],
                )
                .map_err(|error| storage_error("EMBEDDING_WRITE_FAILED", error, true))?;
            transaction
                .execute(
                    "UPDATE chunks SET embedding_model_id = ?1, embedding_status = 'indexed', vector_key = ?2 WHERE chunk_id = ?2",
                    params![model_artifact_id, embedding.chunk_id.to_string()],
                )
                .map_err(|error| storage_error("EMBEDDING_WRITE_FAILED", error, true))?;
            committed += 1;
        }
        transaction
            .commit()
            .map_err(|error| storage_error("DATABASE_COMMIT_FAILED", error, true))?;
        Ok(committed)
    }

    pub fn mark_file_parsing(&self, file_id: &Uuid, revision_id: &Uuid) -> Result<(), AppError> {
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction()
            .map_err(|error| storage_error("DATABASE_TRANSACTION_FAILED", error, true))?;
        let changed = transaction
            .execute(
                "UPDATE files SET parse_status = 'parsing' WHERE file_id = ?1 AND current_revision_id = ?2 AND parse_status IN ('pending', 'failed')",
                params![file_id.to_string(), revision_id.to_string()],
            )
            .map_err(|error| storage_error("INDEX_STATE_UPDATE_FAILED", error, true))?;
        if changed == 0 {
            return Err(AppError::new(
                "INDEX_STALE_REVISION",
                "文件版本已经变化，已丢弃旧解析任务",
                false,
            ));
        }
        transaction
            .execute(
                "UPDATE file_revisions SET parse_status = 'parsing' WHERE revision_id = ?1",
                [revision_id.to_string()],
            )
            .map_err(|error| storage_error("INDEX_STATE_UPDATE_FAILED", error, true))?;
        transaction
            .commit()
            .map_err(|error| storage_error("DATABASE_COMMIT_FAILED", error, true))
    }

    pub fn recover_interrupted_parses(&self) -> Result<u64, AppError> {
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction()
            .map_err(|error| storage_error("DATABASE_TRANSACTION_FAILED", error, true))?;
        let changed = transaction
            .execute(
                "UPDATE files SET parse_status = 'pending' WHERE parse_status = 'parsing'",
                [],
            )
            .map_err(|error| storage_error("INDEX_STATE_UPDATE_FAILED", error, true))?;
        transaction
            .execute(
                "UPDATE file_revisions SET parse_status = 'pending' WHERE parse_status = 'parsing'",
                [],
            )
            .map_err(|error| storage_error("INDEX_STATE_UPDATE_FAILED", error, true))?;
        transaction
            .commit()
            .map_err(|error| storage_error("DATABASE_COMMIT_FAILED", error, true))?;
        Ok(changed as u64)
    }

    pub fn commit_parse_result(
        &self,
        file_id: &Uuid,
        result: &ParseResult,
    ) -> Result<(), AppError> {
        let chunks = chunks_from_nodes(result);
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction()
            .map_err(|error| storage_error("DATABASE_TRANSACTION_FAILED", error, true))?;
        let current_revision: Option<String> = transaction
            .query_row(
                "SELECT current_revision_id FROM files WHERE file_id = ?1",
                [file_id.to_string()],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| storage_error("REVISION_QUERY_FAILED", error, true))?
            .flatten();
        if current_revision.as_deref() != Some(&result.revision_id.to_string()) {
            return Err(AppError::new(
                "INDEX_STALE_REVISION",
                "文件版本已经变化，解析结果未写入当前索引",
                false,
            ));
        }

        transaction
            .execute(
                "DELETE FROM chunks_fts WHERE revision_id = ?1",
                [result.revision_id.to_string()],
            )
            .map_err(|error| storage_error("INDEX_WRITE_FAILED", error, true))?;
        transaction
            .execute(
                "DELETE FROM document_nodes WHERE revision_id = ?1",
                [result.revision_id.to_string()],
            )
            .map_err(|error| storage_error("INDEX_WRITE_FAILED", error, true))?;

        for node in &result.nodes {
            insert_document_node(&transaction, &result.revision_id, node)?;
        }
        for chunk in &chunks {
            let locator_json = serde_json::to_string(&chunk.locator).map_err(|error| {
                AppError::new("INDEX_SERIALIZE_FAILED", error.to_string(), false)
            })?;
            transaction
                .execute(
                    "INSERT INTO chunks (chunk_id, file_id, revision_id, node_id, ordinal, text, normalized_text, token_count, content_hash, language, locator_json, embedding_model_id, embedding_status) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                    params![chunk.chunk_id.to_string(), file_id.to_string(), chunk.revision_id.to_string(), chunk.node_id.to_string(), chunk.ordinal, chunk.text, chunk.normalized_text, chunk.token_count, chunk.content_hash, chunk.language, locator_json, chunk.embedding_model_id, chunk.embedding_status],
                )
                .map_err(|error| storage_error("INDEX_WRITE_FAILED", error, true))?;
            transaction
                .execute(
                    "INSERT INTO chunks_fts (chunk_id, file_id, revision_id, normalized_text) VALUES (?1, ?2, ?3, ?4)",
                    params![chunk.chunk_id.to_string(), file_id.to_string(), chunk.revision_id.to_string(), chunk.normalized_text],
                )
                .map_err(|error| storage_error("INDEX_WRITE_FAILED", error, true))?;
        }

        let parse_status = match result.status {
            ParseOutcome::Parsed => "parsed",
            ParseOutcome::Partial
                if result
                    .warnings
                    .iter()
                    .any(|warning| warning.code == "OCR_REQUIRED") =>
            {
                "ocr_pending"
            }
            ParseOutcome::Partial => "parsed",
            ParseOutcome::Encrypted => "encrypted",
            ParseOutcome::Unsupported => "unsupported",
            ParseOutcome::Failed => "failed",
        };
        let error_code = result
            .error
            .as_ref()
            .map(|error| error.code.as_str())
            .or_else(|| result.warnings.first().map(|warning| warning.code.as_str()));
        let now = Utc::now().to_rfc3339();
        transaction
            .execute(
                "UPDATE file_revisions SET parse_status = ?1, parser_name = ?2, parser_version = ?3, index_version = ?4, completed_at = ?5, error_code = ?6 WHERE revision_id = ?7",
                params![parse_status, result.parser_name, result.parser_version, crate::INDEX_VERSION, now, error_code, result.revision_id.to_string()],
            )
            .map_err(|error| storage_error("INDEX_STATE_UPDATE_FAILED", error, true))?;
        transaction
            .execute(
                "UPDATE files SET parse_status = ?1 WHERE file_id = ?2 AND current_revision_id = ?3",
                params![parse_status, file_id.to_string(), result.revision_id.to_string()],
            )
            .map_err(|error| storage_error("INDEX_STATE_UPDATE_FAILED", error, true))?;
        if matches!(
            parse_status,
            "ocr_pending" | "encrypted" | "unsupported" | "failed"
        ) {
            let event_type = if parse_status == "ocr_pending" {
                InboxEventType::OcrRequired
            } else {
                InboxEventType::ParseFailed
            };
            let triage_status = if parse_status == "ocr_pending" {
                TriageStatus::New
            } else {
                TriageStatus::Error
            };
            let summary = match parse_status {
                "ocr_pending" => "资料需要OCR后才能建立全文索引",
                "encrypted" => "资料已加密，无法读取正文",
                "unsupported" => "当前版本暂不支持解析此资料",
                _ => "资料解析失败，可在收件箱中重试",
            };
            insert_inbox_event(
                &transaction,
                file_id,
                event_type,
                Utc::now(),
                None,
                triage_status,
                Some(summary),
                error_code,
                &format!(
                    "parse:{}:{}:{}",
                    file_id,
                    result.revision_id,
                    event_type.as_storage()
                ),
            )?;
        } else {
            transaction
                .execute(
                    "UPDATE inbox_events SET triage_status = 'reviewed' WHERE file_id = ?1 AND event_type IN ('ocr_required','parse_failed') AND triage_status IN ('new','error')",
                    [file_id.to_string()],
                )
                .map_err(|error| storage_error("INBOX_UPDATE_FAILED", error, true))?;
        }
        transaction
            .commit()
            .map_err(|error| storage_error("DATABASE_COMMIT_FAILED", error, true))
    }

    pub fn search(&self, request: &crate::SearchRequest) -> Result<crate::SearchSession, AppError> {
        self.search_with_semantic(request, None)
    }

    pub fn search_with_semantic(
        &self,
        request: &crate::SearchRequest,
        semantic_query: Option<SemanticQuery<'_>>,
    ) -> Result<crate::SearchSession, AppError> {
        request.validate()?;
        let started_at = std::time::Instant::now();
        let connection = self.connect()?;
        let cursor_fingerprint = search_cursor_fingerprint(
            &connection,
            request,
            semantic_query.as_ref().map(|query| query.model_artifact_id),
        )?;
        let offset =
            crate::indexing::decode_search_cursor(request.cursor.as_deref(), &cursor_fingerprint)?;
        let files = list_files_with_connection(&connection)?;
        let query = request.query.trim().to_lowercase();
        let run_filename = matches!(
            request.mode,
            SearchMode::Filename | SearchMode::Semantic | SearchMode::Hybrid
        );
        let run_fulltext = matches!(
            request.mode,
            SearchMode::Fulltext | SearchMode::Semantic | SearchMode::Hybrid
        );
        let run_semantic = matches!(request.mode, SearchMode::Semantic | SearchMode::Hybrid)
            && semantic_query.is_some();
        let scoped_file_ids = if run_filename || run_semantic {
            Some(collect_scoped_file_ids(
                &connection,
                &files,
                &request.scope,
            )?)
        } else {
            None
        };

        let mut filename_hits = Vec::new();
        if run_filename {
            for file in &files {
                if !scoped_file_ids
                    .as_ref()
                    .is_some_and(|allowed| allowed.contains(&file.file_id))
                {
                    continue;
                }
                let name = file.display_name.to_lowercase();
                let path = file.canonical_path.to_lowercase();
                let (reason, score) = if name == query {
                    ("filename", 1.0)
                } else if name.contains(&query) {
                    ("filename", 0.9)
                } else if path.contains(&query) {
                    ("path", 0.65)
                } else {
                    continue;
                };
                filename_hits.push(RankedHit {
                    file: file.clone(),
                    revision_id: file.current_revision_id,
                    snippet: file.canonical_path.clone(),
                    locator: None,
                    reason,
                    channel_score: score,
                });
            }
            filename_hits.sort_by(|left, right| right.channel_score.total_cmp(&left.channel_score));
        }

        let mut fulltext_hits = if run_fulltext {
            search_fulltext(&connection, &request.query, &request.scope)?
        } else {
            Vec::new()
        };
        fulltext_hits.sort_by(|left, right| right.channel_score.total_cmp(&left.channel_score));

        let semantic_hits = if let Some(semantic_query) = semantic_query
            && run_semantic
        {
            search_semantic(
                &connection,
                &semantic_query,
                scoped_file_ids
                    .as_ref()
                    .expect("semantic scope is prepared"),
            )?
        } else {
            Vec::new()
        };

        let page_size = request.page_size as usize;
        let mut session = crate::fuse_ranked_hits(
            &[filename_hits, fulltext_hits, semantic_hits],
            request.sort,
            offset.saturating_add(page_size).saturating_add(1),
            started_at,
        );
        let has_more = session.results.len() > offset.saturating_add(page_size);
        session.results = session
            .results
            .into_iter()
            .skip(offset)
            .take(page_size)
            .collect();
        session.next_cursor = has_more.then(|| {
            crate::indexing::encode_search_cursor(
                offset.saturating_add(page_size),
                &cursor_fingerprint,
            )
        });
        session.channels.filename = if run_filename {
            crate::SearchChannelState::Completed
        } else {
            crate::SearchChannelState::Unavailable
        };
        session.channels.fulltext = if run_fulltext {
            crate::SearchChannelState::Completed
        } else {
            crate::SearchChannelState::Unavailable
        };
        session.channels.semantic = if run_semantic {
            crate::SearchChannelState::Completed
        } else {
            crate::SearchChannelState::Unavailable
        };
        Ok(session)
    }

    pub fn answer_extractively(
        &self,
        request: &AskRequest,
        semantic_query: Option<SemanticQuery<'_>>,
    ) -> Result<AnswerResult, AppError> {
        request.validate()?;
        let started_at = std::time::Instant::now();
        let search_request = crate::SearchRequest {
            query: request.question.clone(),
            scope: request.scope.clone(),
            mode: crate::SearchMode::Hybrid,
            sort: crate::SearchSort::Relevance,
            page_size: request.retrieval_limit.clamp(10, 30),
            cursor: None,
        };
        let session = self.search_with_semantic(&search_request, semantic_query)?;
        let connection = self.connect()?;
        let mut evidence = Vec::new();
        for result in &session.results {
            if !result
                .match_reasons
                .iter()
                .any(|reason| reason == "fulltext" || reason == "semantic")
            {
                continue;
            }
            let Some(revision_id) = result.revision_id else {
                continue;
            };
            let row = connection
                .query_row(
                    "SELECT chunk_id, node_id, text, locator_json FROM chunks WHERE file_id = ?1 AND revision_id = ?2 AND text = ?3 LIMIT 1",
                    params![result.file_id.to_string(), revision_id.to_string(), result.snippet],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, String>(3)?)),
                )
                .optional()
                .map_err(|error| storage_error("ASK_EVIDENCE_QUERY_FAILED", error, true))?;
            let Some((chunk_id, node_id, quote, locator_json)) = row else {
                continue;
            };
            let locator = serde_json::from_str::<SourceLocator>(&locator_json)
                .map_err(|error| AppError::new("ASK_EVIDENCE_INVALID", error.to_string(), false))?;
            evidence.push((
                crate::EvidenceRef {
                    evidence_id: Uuid::now_v7(),
                    file_id: result.file_id,
                    revision_id,
                    node_id: parse_uuid_value(&node_id)?,
                    chunk_id: parse_uuid_value(&chunk_id)?,
                    quote,
                    locator,
                    retrieval_score: result.scores.fused,
                },
                AnswerSourceFile {
                    file_id: result.file_id,
                    display_name: result.name.clone(),
                    canonical_path: result.path.clone(),
                },
            ));
        }
        Ok(crate::assemble_extractive_answer(
            request, &session, evidence, started_at,
        ))
    }

    pub fn file_preview(
        &self,
        file_id: &Uuid,
        node_limit: usize,
    ) -> Result<crate::FilePreview, AppError> {
        self.file_preview_page(file_id, 0, node_limit, None)
    }

    pub fn file_preview_page(
        &self,
        file_id: &Uuid,
        offset: usize,
        node_limit: usize,
        anchor_node_id: Option<&Uuid>,
    ) -> Result<crate::FilePreview, AppError> {
        if node_limit == 0 || node_limit > 200 || offset > 1_000_000 {
            return Err(AppError::new(
                "PREVIEW_RANGE_INVALID",
                "预览范围无效，每批可读取1到200个内容节点",
                false,
            ));
        }
        let connection = self.connect()?;
        let file = authorized_file_by_id(&connection, file_id)?;
        let Some(revision_id) = file.current_revision_id else {
            return Ok(crate::FilePreview {
                file,
                revision_id: None,
                nodes: vec![],
                offset: 0,
                next_offset: None,
                anchor_node_id: None,
                truncated: false,
            });
        };
        let effective_offset = if let Some(anchor_node_id) = anchor_node_id {
            let anchor_ordinal = connection
                .query_row(
                    "SELECT ordinal FROM document_nodes WHERE node_id = ?1 AND revision_id = ?2",
                    params![anchor_node_id.to_string(), revision_id.to_string()],
                    |row| row.get::<_, u64>(0),
                )
                .optional()
                .map_err(|error| storage_error("PREVIEW_QUERY_FAILED", error, true))?
                .ok_or_else(|| {
                    AppError::new(
                        "PREVIEW_ANCHOR_NOT_FOUND",
                        "引用位置已不属于资料的当前版本，请重新检索",
                        true,
                    )
                })?;
            connection
                .query_row(
                    "SELECT COUNT(*) FROM document_nodes WHERE revision_id = ?1 AND ordinal < ?2",
                    params![revision_id.to_string(), anchor_ordinal],
                    |row| row.get::<_, usize>(0),
                )
                .map_err(|error| storage_error("PREVIEW_QUERY_FAILED", error, true))?
                .saturating_sub(10)
        } else {
            offset
        };
        let mut statement = connection
            .prepare(
                "SELECT node_id, parent_id, ordinal, node_type, text, table_json, locator_json, heading_path_json FROM document_nodes WHERE revision_id = ?1 ORDER BY ordinal LIMIT ?2 OFFSET ?3",
            )
            .map_err(|error| storage_error("PREVIEW_QUERY_FAILED", error, true))?;
        let mut nodes = statement
            .query_map(
                params![
                    revision_id.to_string(),
                    node_limit.saturating_add(1) as u64,
                    effective_offset as u64
                ],
                document_node_from_row,
            )
            .map_err(|error| storage_error("PREVIEW_QUERY_FAILED", error, true))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| storage_error("PREVIEW_QUERY_FAILED", error, true))?;
        let truncated = nodes.len() > node_limit;
        nodes.truncate(node_limit);
        let next_offset = truncated.then_some((effective_offset + nodes.len()) as u32);
        Ok(crate::FilePreview {
            file,
            revision_id: Some(revision_id),
            nodes,
            offset: effective_offset as u32,
            next_offset,
            anchor_node_id: anchor_node_id.copied(),
            truncated,
        })
    }

    pub fn authorized_file_path(&self, file_id: &Uuid) -> Result<PathBuf, AppError> {
        let connection = self.connect()?;
        let file = authorized_file_by_id(&connection, file_id)?;
        if file.availability != crate::Availability::Present {
            return Err(AppError::new(
                "FILE_UNAVAILABLE",
                "文件当前不在电脑上，无法打开",
                false,
            ));
        }
        Ok(PathBuf::from(file.canonical_path))
    }

    pub fn append_log(&self, event: &LogEventInput<'_>) -> Result<(), AppError> {
        let connection = self.connect()?;
        let safe_fields = sanitize_log_value(event.fields, None);
        connection
            .execute(
                "INSERT INTO log_events (log_id, level, component, event_name, job_id, root_id, file_id, fields_json, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![Uuid::now_v7().to_string(), event.level, event.component, event.event_name, event.job_id.map(Uuid::to_string), event.root_id.map(Uuid::to_string), event.file_id.map(Uuid::to_string), serde_json::to_string(&safe_fields).map_err(|error| AppError::new("LOG_SERIALIZE_FAILED", error.to_string(), false))?, Utc::now().to_rfc3339()],
            )
            .map_err(|error| storage_error("LOG_WRITE_FAILED", error, true))?;
        connection
            .execute(
                "DELETE FROM log_events WHERE log_id IN (SELECT log_id FROM log_events ORDER BY created_at DESC LIMIT -1 OFFSET 10000)",
                [],
            )
            .map_err(|error| storage_error("LOG_PRUNE_FAILED", error, true))?;
        Ok(())
    }

    pub fn degradation_state(&self) -> Result<DegradationState, AppError> {
        let connection = self.connect()?;
        Self::degradation_state_with_connection(&connection)
    }

    pub fn reconcile_degradation_state(
        &self,
        desired_level: DegradationLevel,
        triggers: Vec<String>,
    ) -> Result<DegradationState, AppError> {
        let connection = self.connect()?;
        Self::reconcile_degradation_state_with_connection(&connection, desired_level, triggers)
    }

    fn degradation_state_with_connection(
        connection: &Connection,
    ) -> Result<DegradationState, AppError> {
        let state_json = connection
            .query_row(
                "SELECT state_json FROM degradation_states WHERE state_id = 1",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| storage_error("DEGRADATION_STATE_QUERY_FAILED", error, true))?;
        let Some(state_json) = state_json else {
            return Ok(DegradationState::full());
        };
        let state = serde_json::from_str::<DegradationState>(&state_json).map_err(|error| {
            AppError::new("SCHEMA_INVALID_DEGRADATION_STATE", error.to_string(), false)
        })?;
        state.validate()?;
        Ok(state)
    }

    fn reconcile_degradation_state_with_connection(
        connection: &Connection,
        desired_level: DegradationLevel,
        triggers: Vec<String>,
    ) -> Result<DegradationState, AppError> {
        Self::reconcile_degradation_state_at(connection, desired_level, triggers, Utc::now())
    }

    fn reconcile_degradation_state_at(
        connection: &Connection,
        desired_level: DegradationLevel,
        triggers: Vec<String>,
        now: DateTime<Utc>,
    ) -> Result<DegradationState, AppError> {
        let current = Self::degradation_state_with_connection(connection)?;
        if current.manual_override {
            return Ok(current);
        }
        if degradation_rank(desired_level) < degradation_rank(current.level)
            && current
                .recover_after
                .is_some_and(|recover_after| now < recover_after)
        {
            return Ok(current);
        }
        let next_level = match (current.level, desired_level) {
            (DegradationLevel::Full, DegradationLevel::Core) => DegradationLevel::Balanced,
            (DegradationLevel::Core, DegradationLevel::Full) => DegradationLevel::Balanced,
            (_, level) => level,
        };
        let next = if next_level == DegradationLevel::Full {
            DegradationState::full()
        } else {
            DegradationState {
                level: next_level,
                triggers: if triggers.is_empty() {
                    vec!["系统仍在确认资源恢复状态".to_owned()]
                } else {
                    triggers
                },
                disabled_features: match next_level {
                    DegradationLevel::Balanced => {
                        vec!["background_similarity".to_owned(), "reranking".to_owned()]
                    }
                    DegradationLevel::Core => vec![
                        "background_similarity".to_owned(),
                        "reranking".to_owned(),
                        "semantic_indexing".to_owned(),
                        "generation".to_owned(),
                        "background_ocr".to_owned(),
                    ],
                    DegradationLevel::Full => Vec::new(),
                },
                entered_at: if current.level == next_level {
                    current.entered_at.or(Some(now))
                } else {
                    Some(now)
                },
                recover_after: Some(now + chrono::Duration::minutes(5)),
                manual_override: false,
            }
        };
        next.validate()?;
        let state_json = serde_json::to_string(&next).map_err(|error| {
            AppError::new("SCHEMA_INVALID_DEGRADATION_STATE", error.to_string(), false)
        })?;
        connection
            .execute(
                "INSERT INTO degradation_states (state_id, state_json, updated_at) VALUES (1, ?1, ?2) ON CONFLICT(state_id) DO UPDATE SET state_json = excluded.state_json, updated_at = excluded.updated_at",
                params![state_json, now.to_rfc3339()],
            )
            .map_err(|error| storage_error("DEGRADATION_STATE_WRITE_FAILED", error, true))?;
        Ok(next)
    }

    pub fn maintenance_snapshot(&self) -> Result<MaintenanceSnapshot, AppError> {
        let connection = self.connect()?;
        connection
            .query_row("SELECT 1", [], |row| row.get::<_, i64>(0))
            .map_err(|error| storage_error("DATABASE_HEALTH_CHECK_FAILED", error, true))?;
        let indexed_files = count_query(
            &connection,
            "SELECT COUNT(*) FROM files WHERE parse_status = 'parsed'",
        )?;
        let searchable_chunks = count_query(&connection, "SELECT COUNT(*) FROM chunks")?;
        let embedded_chunks = count_query(&connection, "SELECT COUNT(*) FROM chunk_embeddings")?;
        let pending_files = count_query(
            &connection,
            "SELECT COUNT(*) FROM files WHERE parse_status IN ('pending','parsing','ocr_pending')",
        )?;
        let failed_files = count_query(
            &connection,
            "SELECT COUNT(*) FROM files WHERE parse_status IN ('failed','unsupported','encrypted')",
        )?;
        let active_jobs = count_query(
            &connection,
            "SELECT COUNT(*) FROM jobs WHERE status IN ('queued','running','paused','awaiting_user')",
        )?;
        let log_events = count_query(&connection, "SELECT COUNT(*) FROM log_events")?;
        let database_size_bytes = fs::metadata(&self.database_path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        let mut checks = vec![
            HealthCheckItem {
                key: "database".into(),
                label: "本地数据库".into(),
                status: "passed".into(),
                detail: "连接正常；完整性检查按需运行".into(),
            },
            HealthCheckItem {
                key: "schema".into(),
                label: "数据结构".into(),
                status: "passed".into(),
                detail: format!("版本 {CURRENT_SCHEMA_VERSION}"),
            },
            HealthCheckItem {
                key: "source_readonly".into(),
                label: "源文件保护".into(),
                status: "passed".into(),
                detail: "维护操作只作用于拾忆索引与日志".into(),
            },
        ];
        if failed_files > 0 {
            checks.push(HealthCheckItem {
                key: "parse_failures".into(),
                label: "解析异常".into(),
                status: "warning".into(),
                detail: format!("{failed_files} 份资料需要检查"),
            });
        }
        let (desired_level, degradation_reasons) = if pending_files > 500 || active_jobs > 3 {
            (
                DegradationLevel::Balanced,
                vec!["后台处理队列较长，已优先保证搜索与预览".to_owned()],
            )
        } else {
            (DegradationLevel::Full, Vec::new())
        };
        let degradation = Self::reconcile_degradation_state_with_connection(
            &connection,
            desired_level,
            degradation_reasons,
        )?;
        let degradation_level = match degradation.level {
            DegradationLevel::Full => "full",
            DegradationLevel::Balanced => "balanced",
            DegradationLevel::Core => "core",
        }
        .to_owned();
        Ok(MaintenanceSnapshot {
            schema_version: CURRENT_SCHEMA_VERSION,
            database_size_bytes,
            indexed_files,
            searchable_chunks,
            embedded_chunks,
            pending_files,
            failed_files,
            active_jobs,
            log_events,
            degradation_level,
            degradation_reasons: degradation.triggers,
            checks,
            checked_at: Utc::now(),
        })
    }

    pub fn maintenance_check(
        &self,
        level: &str,
    ) -> Result<crate::MaintenanceCheckResult, AppError> {
        let pragma = match level {
            "quick" => "PRAGMA quick_check",
            "full" => "PRAGMA integrity_check",
            _ => {
                return Err(AppError::new(
                    "MAINTENANCE_CHECK_LEVEL_INVALID",
                    "维护检查级别必须是quick或full",
                    false,
                ));
            }
        };
        let started = std::time::Instant::now();
        let connection = self.connect()?;
        let database_result = connection
            .query_row(pragma, [], |row| row.get::<_, String>(0))
            .map_err(|error| storage_error("DATABASE_HEALTH_CHECK_FAILED", error, true))?;
        Ok(crate::MaintenanceCheckResult {
            level: level.to_owned(),
            database_result,
            elapsed_ms: started.elapsed().as_millis() as u64,
            source_files_modified: false,
        })
    }

    pub fn index_activity_stats(&self) -> Result<IndexActivityStats, AppError> {
        let connection = self.connect()?;
        let authorized_file = "EXISTS (SELECT 1 FROM file_root_memberships m JOIN roots r ON r.root_id = m.root_id WHERE m.file_id = f.file_id AND r.enabled = 1)";
        let discovered_files = count_query(
            &connection,
            &format!(
                "SELECT COUNT(*) FROM files f WHERE f.availability = 'present' AND {authorized_file}"
            ),
        )?;
        let parsed_files = count_query(
            &connection,
            &format!(
                "SELECT COUNT(*) FROM files f WHERE f.availability = 'present' AND f.parse_status = 'parsed' AND {authorized_file}"
            ),
        )?;
        let searchable_files = count_query(
            &connection,
            &format!(
                "SELECT COUNT(DISTINCT c.file_id) FROM chunks c JOIN files f ON f.file_id = c.file_id WHERE f.availability = 'present' AND f.current_revision_id = c.revision_id AND {authorized_file}"
            ),
        )?;
        let embedded_files = count_query(
            &connection,
            &format!(
                "SELECT COUNT(DISTINCT e.file_id) FROM chunk_embeddings e JOIN files f ON f.file_id = e.file_id WHERE f.availability = 'present' AND f.current_revision_id = e.revision_id AND {authorized_file}"
            ),
        )?;
        let ocr_pages = count_query(
            &connection,
            &format!(
                "SELECT COUNT(DISTINCT f.file_id || ':' || COALESCE(CAST(json_extract(n.locator_json, '$.page_no') AS TEXT), '1')) FROM document_nodes n JOIN file_revisions revision ON revision.revision_id = n.revision_id JOIN files f ON f.file_id = revision.file_id WHERE f.availability = 'present' AND f.current_revision_id = n.revision_id AND n.node_type = 'ocr_line' AND {authorized_file}"
            ),
        )?;
        Ok(IndexActivityStats {
            discovered_files,
            searchable_files,
            parsed_files,
            embedded_files,
            ocr_pages,
        })
    }

    pub fn list_logs(&self, limit: u32) -> Result<Vec<AppLogRecord>, AppError> {
        self.query_logs(&LogQuery {
            cursor: None,
            page_size: limit,
        })
        .map(|page| page.items)
    }

    pub fn query_logs(&self, request: &LogQuery) -> Result<LogPage, AppError> {
        request.validate()?;
        let offset = request.offset()?;
        let connection = self.connect()?;
        let total = connection
            .query_row("SELECT COUNT(*) FROM log_events", [], |row| {
                row.get::<_, u64>(0)
            })
            .map_err(|error| storage_error("LOG_QUERY_FAILED", error, true))?;
        let mut statement = connection.prepare("SELECT log_id, level, component, event_name, fields_json, created_at FROM log_events ORDER BY created_at DESC, log_id DESC LIMIT ?1 OFFSET ?2")
            .map_err(|error| storage_error("LOG_QUERY_FAILED", error, true))?;
        let items = statement
            .query_map(params![u64::from(request.page_size), offset], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                ))
            })
            .map_err(|error| storage_error("LOG_QUERY_FAILED", error, true))?
            .map(|row| {
                let (log_id, level, component, event_name, fields_json, created_at) =
                    row.map_err(|error| storage_error("LOG_QUERY_FAILED", error, true))?;
                Ok(AppLogRecord {
                    log_id,
                    level,
                    component,
                    event_name,
                    fields: serde_json::from_str(&fields_json).map_err(|error| {
                        AppError::new("LOG_DATA_INVALID", error.to_string(), false)
                    })?,
                    created_at: DateTime::parse_from_rfc3339(&created_at)
                        .map_err(|error| {
                            AppError::new("LOG_DATA_INVALID", error.to_string(), false)
                        })?
                        .with_timezone(&Utc),
                })
            })
            .collect::<Result<Vec<_>, AppError>>()?;
        let consumed = offset.saturating_add(items.len() as u64);
        Ok(LogPage {
            items,
            next_cursor: (consumed < total).then(|| consumed.to_string()),
            total,
        })
    }

    pub fn clear_logs(&self) -> Result<u64, AppError> {
        let connection = self.connect()?;
        connection
            .execute("DELETE FROM log_events", [])
            .map(|value| value as u64)
            .map_err(|error| storage_error("LOG_CLEAR_FAILED", error, true))
    }

    pub fn rebuild_index(&self, confirmation: &str) -> Result<IndexRebuildResult, AppError> {
        crate::validate_rebuild_confirmation(confirmation)?;
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction()
            .map_err(|error| storage_error("DATABASE_TRANSACTION_FAILED", error, true))?;
        let removed_embeddings =
            count_query(&transaction, "SELECT COUNT(*) FROM chunk_embeddings")?;
        let removed_chunks = count_query(&transaction, "SELECT COUNT(*) FROM chunks")?;
        let removed_nodes = count_query(&transaction, "SELECT COUNT(*) FROM document_nodes")?;
        let reset_files = count_query(
            &transaction,
            "SELECT COUNT(*) FROM files WHERE current_revision_id IS NOT NULL",
        )?;
        transaction
            .execute("DELETE FROM chunk_embeddings", [])
            .map_err(|error| storage_error("INDEX_REBUILD_FAILED", error, true))?;
        transaction
            .execute("DELETE FROM chunks_fts", [])
            .map_err(|error| storage_error("INDEX_REBUILD_FAILED", error, true))?;
        transaction
            .execute("DELETE FROM chunks", [])
            .map_err(|error| storage_error("INDEX_REBUILD_FAILED", error, true))?;
        transaction
            .execute("DELETE FROM document_nodes", [])
            .map_err(|error| storage_error("INDEX_REBUILD_FAILED", error, true))?;
        transaction.execute("UPDATE file_revisions SET parse_status = 'pending', parser_name = NULL, parser_version = NULL, index_version = NULL, completed_at = NULL, error_code = NULL WHERE revision_id IN (SELECT current_revision_id FROM files WHERE current_revision_id IS NOT NULL)", []).map_err(|error| storage_error("INDEX_REBUILD_FAILED", error, true))?;
        transaction
            .execute(
                "UPDATE files SET parse_status = 'pending' WHERE current_revision_id IS NOT NULL",
                [],
            )
            .map_err(|error| storage_error("INDEX_REBUILD_FAILED", error, true))?;
        transaction
            .commit()
            .map_err(|error| storage_error("DATABASE_COMMIT_FAILED", error, true))?;
        Ok(IndexRebuildResult {
            reset_files,
            removed_nodes,
            removed_chunks,
            removed_embeddings,
            source_files_modified: false,
        })
    }

    pub fn record_file_events(
        &self,
        root_id: &Uuid,
        events: &[FileSystemEvent],
    ) -> Result<(), AppError> {
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction()
            .map_err(|error| storage_error("DATABASE_TRANSACTION_FAILED", error, true))?;
        for event in events {
            let fingerprint = format!(
                "{}|{}|{}|{}|{}",
                root_id,
                event.event_type.as_str(),
                path_key_for_storage(&event.observed_path),
                event
                    .previous_path
                    .as_deref()
                    .map(path_key_for_storage)
                    .unwrap_or_default(),
                event.observed_at.to_rfc3339()
            );
            transaction
                .execute(
                    "INSERT OR IGNORE INTO file_events (event_id, event_fingerprint, root_id, event_type, observed_path, previous_path, observed_at, state) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'pending')",
                    params![Uuid::now_v7().to_string(), fingerprint, root_id.to_string(), event.event_type.as_str(), event.observed_path, event.previous_path, event.observed_at.to_rfc3339()],
                )
                .map_err(|error| storage_error("FILE_EVENT_WRITE_FAILED", error, true))?;
        }
        transaction
            .commit()
            .map_err(|error| storage_error("DATABASE_COMMIT_FAILED", error, true))
    }

    pub fn mark_file_events_coalesced(&self, root_id: &Uuid) -> Result<(), AppError> {
        let connection = self.connect()?;
        connection
            .execute(
                "UPDATE file_events SET state = 'coalesced', coalesced_at = ?1 WHERE root_id = ?2 AND state = 'pending'",
                params![Utc::now().to_rfc3339(), root_id.to_string()],
            )
            .map_err(|error| storage_error("FILE_EVENT_UPDATE_FAILED", error, true))?;
        Ok(())
    }
}

fn path_key_for_storage(path: &str) -> String {
    path.replace('/', "\\")
        .trim_end_matches('\\')
        .to_ascii_lowercase()
}

fn count_query(connection: &Connection, query: &str) -> Result<u64, AppError> {
    connection
        .query_row(query, [], |row| row.get::<_, u64>(0))
        .map_err(|error| storage_error("DATABASE_COUNT_QUERY_FAILED", error, true))
}

fn sanitize_log_value(value: &serde_json::Value, field_name: Option<&str>) -> serde_json::Value {
    let sensitive_field = field_name.is_some_and(|name| {
        let name = name.to_ascii_lowercase();
        ["path", "text", "content", "body", "quote"]
            .iter()
            .any(|token| name.contains(token))
    });
    if sensitive_field {
        return serde_json::Value::String("[redacted]".to_owned());
    }
    match value {
        serde_json::Value::String(text)
            if text.starts_with("\\\\")
                || text
                    .as_bytes()
                    .get(1..3)
                    .is_some_and(|bytes| bytes == b":\\" || bytes == b":/") =>
        {
            serde_json::Value::String("[redacted_path]".to_owned())
        }
        serde_json::Value::Array(items) => serde_json::Value::Array(
            items
                .iter()
                .map(|item| sanitize_log_value(item, None))
                .collect(),
        ),
        serde_json::Value::Object(fields) => serde_json::Value::Object(
            fields
                .iter()
                .map(|(key, value)| (key.clone(), sanitize_log_value(value, Some(key))))
                .collect(),
        ),
        _ => value.clone(),
    }
}

fn degradation_rank(level: DegradationLevel) -> u8 {
    match level {
        DegradationLevel::Full => 0,
        DegradationLevel::Balanced => 1,
        DegradationLevel::Core => 2,
    }
}

fn insert_document_node(
    transaction: &Transaction<'_>,
    revision_id: &Uuid,
    node: &DocumentNode,
) -> Result<(), AppError> {
    let locator_json = serde_json::to_string(&node.locator)
        .map_err(|error| AppError::new("INDEX_SERIALIZE_FAILED", error.to_string(), false))?;
    let heading_path_json = serde_json::to_string(&node.heading_path)
        .map_err(|error| AppError::new("INDEX_SERIALIZE_FAILED", error.to_string(), false))?;
    let table_json = node
        .table_data
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|error| AppError::new("INDEX_SERIALIZE_FAILED", error.to_string(), false))?;
    transaction
        .execute(
            "INSERT INTO document_nodes (node_id, revision_id, parent_id, ordinal, node_type, locator_json, heading_path_json, text, table_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![node.node_id.to_string(), revision_id.to_string(), node.parent_id.map(|value| value.to_string()), node.ordinal, node.node_type, locator_json, heading_path_json, node.text, table_json],
        )
        .map_err(|error| storage_error("INDEX_WRITE_FAILED", error, true))?;
    Ok(())
}

fn list_files_with_connection(connection: &Connection) -> Result<Vec<FileRecord>, AppError> {
    let mut statement = connection
        .prepare(
            "SELECT file_id, volume_id, canonical_path, display_name, extension, mime_type, size_bytes, fs_created_at, modified_at, windows_file_id, content_sha256, availability, current_revision_id, parse_status, first_seen_at, last_seen_at FROM files ORDER BY last_seen_at DESC",
        )
        .map_err(|error| storage_error("FILE_QUERY_FAILED", error, true))?;
    statement
        .query_map([], file_from_row)
        .map_err(|error| storage_error("FILE_QUERY_FAILED", error, true))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| storage_error("FILE_QUERY_FAILED", error, true))
}

fn file_is_authorized(connection: &Connection, file_id: &Uuid) -> Result<bool, AppError> {
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM file_root_memberships m JOIN roots r ON r.root_id = m.root_id WHERE m.file_id = ?1 AND r.enabled = 1)",
            [file_id.to_string()],
            |row| row.get::<_, i64>(0),
        )
        .map(|value| value != 0)
        .map_err(|error| storage_error("FILE_AUTHORIZATION_QUERY_FAILED", error, true))
}

fn file_belongs_to_any_root(
    connection: &Connection,
    file_id: &Uuid,
    root_ids: &[Uuid],
) -> Result<bool, AppError> {
    let expected = root_ids.iter().map(Uuid::to_string).collect::<HashSet<_>>();
    let mut statement = connection
        .prepare("SELECT root_id FROM file_root_memberships WHERE file_id = ?1")
        .map_err(|error| storage_error("MEMBERSHIP_QUERY_FAILED", error, true))?;
    let rows = statement
        .query_map([file_id.to_string()], |row| row.get::<_, String>(0))
        .map_err(|error| storage_error("MEMBERSHIP_QUERY_FAILED", error, true))?;
    for root_id in rows {
        if expected.contains(
            &root_id.map_err(|error| storage_error("MEMBERSHIP_QUERY_FAILED", error, true))?,
        ) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn collection_rule_matches(
    connection: &Connection,
    rule: &CollectionRule,
    file: &FileRecord,
) -> Result<bool, AppError> {
    let has_metadata_conditions = !rule.extensions.is_empty()
        || !rule.filename_keywords.is_empty()
        || !rule.path_keywords.is_empty()
        || !rule.parse_statuses.is_empty()
        || rule.modified_within_days.is_some();
    let metadata_matches = has_metadata_conditions && rule.matches_metadata(file, Utc::now());
    let has_text_conditions = !rule.text_keywords.is_empty();
    let mut text_matches = false;
    if has_text_conditions && let Some(revision_id) = file.current_revision_id {
        for keyword in &rule.text_keywords {
            let pattern = format!("%{}%", keyword.trim().to_lowercase());
            let found = connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM chunks WHERE revision_id = ?1 AND lower(text) LIKE ?2)",
                    params![revision_id.to_string(), pattern],
                    |row| row.get::<_, i64>(0),
                )
                .map_err(|error| storage_error("COLLECTION_QUERY_FAILED", error, true))?
                != 0;
            if found {
                text_matches = true;
                break;
            }
        }
    }
    Ok(match rule.operator {
        crate::RuleOperator::All => {
            (!has_metadata_conditions || metadata_matches) && (!has_text_conditions || text_matches)
        }
        crate::RuleOperator::Any => {
            (has_metadata_conditions && metadata_matches) || (has_text_conditions && text_matches)
        }
    })
}

fn collection_rows(connection: &Connection) -> Result<Vec<CollectionRecord>, AppError> {
    let raw = {
        let mut statement = connection
            .prepare(
                "SELECT collection_id, name, description, icon, color, kind, rule_json, built_in, created_at, updated_at FROM collections ORDER BY built_in DESC, created_at",
            )
            .map_err(|error| storage_error("COLLECTION_QUERY_FAILED", error, true))?;
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, String>(9)?,
                ))
            })
            .map_err(|error| storage_error("COLLECTION_QUERY_FAILED", error, true))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| storage_error("COLLECTION_QUERY_FAILED", error, true))?
    };
    let mut collections = Vec::with_capacity(raw.len());
    for (
        collection_id,
        name,
        description,
        icon,
        color,
        kind,
        rule_json,
        built_in,
        created_at,
        updated_at,
    ) in raw
    {
        let collection_id = parse_uuid_value(&collection_id)?;
        let kind = CollectionKind::from_storage(&kind);
        let rule = rule_json
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(|error| AppError::new("COLLECTION_RULE_INVALID", error.to_string(), false))?;
        let file_count = match kind {
            CollectionKind::Manual => connection
                .query_row(
                    "SELECT COUNT(*) FROM collection_memberships m WHERE m.collection_id = ?1 AND EXISTS (SELECT 1 FROM file_root_memberships fm JOIN roots r ON r.root_id = fm.root_id WHERE fm.file_id = m.file_id AND r.enabled = 1)",
                    [collection_id.to_string()],
                    |row| row.get::<_, u64>(0),
                )
                .map_err(|error| storage_error("COLLECTION_QUERY_FAILED", error, true))?,
            CollectionKind::Rule => {
                let rule = rule.as_ref().ok_or_else(|| {
                    AppError::new("COLLECTION_RULE_INVALID", "规则集合缺少规则", false)
                })?;
                count_files_for_rule(connection, rule)?
            }
        };
        collections.push(CollectionRecord {
            collection_id,
            name,
            description,
            icon,
            color,
            kind,
            rule,
            file_count,
            built_in: built_in != 0,
            created_at: DateTime::parse_from_rfc3339(&created_at)
                .map_err(|error| {
                    AppError::new("COLLECTION_DATA_INVALID", error.to_string(), false)
                })?
                .with_timezone(&Utc),
            updated_at: DateTime::parse_from_rfc3339(&updated_at)
                .map_err(|error| {
                    AppError::new("COLLECTION_DATA_INVALID", error.to_string(), false)
                })?
                .with_timezone(&Utc),
        });
    }
    Ok(collections)
}

const FILE_SELECT_WITH_ALIAS: &str = "SELECT f.file_id, f.volume_id, f.canonical_path, f.display_name, f.extension, f.mime_type, f.size_bytes, f.fs_created_at, f.modified_at, f.windows_file_id, f.content_sha256, f.availability, f.current_revision_id, f.parse_status, f.first_seen_at, f.last_seen_at FROM files f";
const AUTHORIZED_FILE_SQL: &str = "EXISTS (SELECT 1 FROM file_root_memberships m JOIN roots r ON r.root_id = m.root_id WHERE m.file_id = f.file_id AND r.enabled = 1)";

fn count_manual_collection_files(
    connection: &Connection,
    collection_id: &Uuid,
) -> Result<u64, AppError> {
    connection
        .query_row(
            &format!(
                "SELECT COUNT(*) FROM files f JOIN collection_memberships cm ON cm.file_id = f.file_id WHERE cm.collection_id = ?1 AND {AUTHORIZED_FILE_SQL}"
            ),
            [collection_id.to_string()],
            |row| row.get(0),
        )
        .map_err(|error| storage_error("COLLECTION_QUERY_FAILED", error, true))
}

fn query_manual_collection_files(
    connection: &Connection,
    collection_id: &Uuid,
    offset: u64,
    page_size: u32,
) -> Result<Vec<FileRecord>, AppError> {
    let sql = format!(
        "{FILE_SELECT_WITH_ALIAS} JOIN collection_memberships cm ON cm.file_id = f.file_id WHERE cm.collection_id = ?1 AND {AUTHORIZED_FILE_SQL} ORDER BY f.last_seen_at DESC LIMIT ?2 OFFSET ?3"
    );
    let mut statement = connection
        .prepare(&sql)
        .map_err(|error| storage_error("COLLECTION_QUERY_FAILED", error, true))?;
    statement
        .query_map(
            params![
                collection_id.to_string(),
                i64::from(page_size),
                offset as i64
            ],
            file_from_row,
        )
        .map_err(|error| storage_error("COLLECTION_QUERY_FAILED", error, true))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| storage_error("COLLECTION_QUERY_FAILED", error, true))
}

fn collection_rule_predicate(rule: &CollectionRule) -> (String, Vec<SqlValue>) {
    let mut conditions = Vec::new();
    let mut values = Vec::new();
    if !rule.extensions.is_empty() {
        conditions.push(format!(
            "lower(f.extension) IN ({})",
            vec!["?"; rule.extensions.len()].join(",")
        ));
        values.extend(
            rule.extensions
                .iter()
                .map(|value| SqlValue::Text(value.trim_start_matches('.').to_lowercase())),
        );
    }
    if !rule.filename_keywords.is_empty() {
        conditions.push(format!(
            "({})",
            vec!["lower(f.display_name) LIKE ?"; rule.filename_keywords.len()].join(" OR ")
        ));
        values.extend(
            rule.filename_keywords
                .iter()
                .map(|value| SqlValue::Text(format!("%{}%", value.to_lowercase()))),
        );
    }
    if !rule.path_keywords.is_empty() {
        conditions.push(format!(
            "({})",
            vec!["lower(f.canonical_path) LIKE ?"; rule.path_keywords.len()].join(" OR ")
        ));
        values.extend(
            rule.path_keywords
                .iter()
                .map(|value| SqlValue::Text(format!("%{}%", value.to_lowercase()))),
        );
    }
    if !rule.parse_statuses.is_empty() {
        conditions.push(format!(
            "f.parse_status IN ({})",
            vec!["?"; rule.parse_statuses.len()].join(",")
        ));
        values.extend(
            rule.parse_statuses
                .iter()
                .map(|value| SqlValue::Text(value.as_str().to_owned())),
        );
    }
    if let Some(days) = rule.modified_within_days {
        conditions.push("f.modified_at >= ?".to_owned());
        values.push(SqlValue::Text(
            (Utc::now() - chrono::Duration::days(i64::from(days))).to_rfc3339(),
        ));
    }
    if !rule.text_keywords.is_empty() {
        conditions.push(format!(
            "EXISTS (SELECT 1 FROM chunks rc WHERE rc.revision_id = f.current_revision_id AND ({}))",
            vec!["lower(rc.text) LIKE ?"; rule.text_keywords.len()].join(" OR ")
        ));
        values.extend(
            rule.text_keywords
                .iter()
                .map(|value| SqlValue::Text(format!("%{}%", value.to_lowercase()))),
        );
    }
    let separator = match rule.operator {
        crate::RuleOperator::All => " AND ",
        crate::RuleOperator::Any => " OR ",
    };
    (conditions.join(separator), values)
}

fn count_files_for_rule(connection: &Connection, rule: &CollectionRule) -> Result<u64, AppError> {
    rule.validate()?;
    let (predicate, values) = collection_rule_predicate(rule);
    connection
        .query_row(
            &format!("SELECT COUNT(*) FROM files f WHERE {AUTHORIZED_FILE_SQL} AND ({predicate})"),
            params_from_iter(values.iter()),
            |row| row.get(0),
        )
        .map_err(|error| storage_error("COLLECTION_QUERY_FAILED", error, true))
}

fn query_files_for_rule(
    connection: &Connection,
    rule: &CollectionRule,
    offset: u64,
    page_size: u32,
) -> Result<Vec<FileRecord>, AppError> {
    rule.validate()?;
    let (predicate, mut values) = collection_rule_predicate(rule);
    values.push(SqlValue::Integer(i64::from(page_size)));
    values.push(SqlValue::Integer(offset as i64));
    let sql = format!(
        "{FILE_SELECT_WITH_ALIAS} WHERE {AUTHORIZED_FILE_SQL} AND ({predicate}) ORDER BY f.last_seen_at DESC LIMIT ? OFFSET ?"
    );
    let mut statement = connection
        .prepare(&sql)
        .map_err(|error| storage_error("COLLECTION_QUERY_FAILED", error, true))?;
    statement
        .query_map(params_from_iter(values.iter()), file_from_row)
        .map_err(|error| storage_error("COLLECTION_QUERY_FAILED", error, true))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| storage_error("COLLECTION_QUERY_FAILED", error, true))
}

fn collection_by_id(
    connection: &Connection,
    collection_id: &Uuid,
) -> Result<CollectionRecord, AppError> {
    collection_rows(connection)?
        .into_iter()
        .find(|collection| collection.collection_id == *collection_id)
        .ok_or_else(|| AppError::new("COLLECTION_NOT_FOUND", "智能集合不存在", false))
}

fn hash_file_sha256(path: &PathBuf) -> Result<String, AppError> {
    let mut file = File::open(path)
        .map_err(|error| AppError::new("FILE_HASH_FAILED", error.to_string(), true))?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| AppError::new("FILE_HASH_FAILED", error.to_string(), true))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn insert_relation(
    transaction: &Transaction<'_>,
    left_file_id: Uuid,
    right_file_id: Uuid,
    relation_type: RelationType,
    confidence: f64,
    reasons: &[String],
) -> Result<(), AppError> {
    let mut left = left_file_id.to_string();
    let mut right = right_file_id.to_string();
    if left > right {
        std::mem::swap(&mut left, &mut right);
    }
    let now = Utc::now().to_rfc3339();
    let reasons_json = serde_json::to_string(reasons)
        .map_err(|error| AppError::new("RELATION_DATA_INVALID", error.to_string(), false))?;
    transaction
        .execute(
            "INSERT INTO file_relations (relation_id, left_file_id, right_file_id, relation_type, confidence, reasons_json, review_status, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'suggested', ?7, ?7) ON CONFLICT(left_file_id, right_file_id, relation_type) DO UPDATE SET confidence = excluded.confidence, reasons_json = excluded.reasons_json, updated_at = excluded.updated_at",
            params![Uuid::now_v7().to_string(), left, right, relation_type.as_storage(), confidence, reasons_json, now],
        )
        .map_err(|error| storage_error("RELATION_REFRESH_FAILED", error, true))?;
    Ok(())
}

fn authorized_file_by_id(connection: &Connection, file_id: &Uuid) -> Result<FileRecord, AppError> {
    connection
        .query_row(
            "SELECT f.file_id, f.volume_id, f.canonical_path, f.display_name, f.extension, f.mime_type, f.size_bytes, f.fs_created_at, f.modified_at, f.windows_file_id, f.content_sha256, f.availability, f.current_revision_id, f.parse_status, f.first_seen_at, f.last_seen_at FROM files f WHERE f.file_id = ?1 AND EXISTS (SELECT 1 FROM file_root_memberships m JOIN roots r ON r.root_id = m.root_id WHERE m.file_id = f.file_id AND r.enabled = 1)",
            [file_id.to_string()],
            file_from_row,
        )
        .map_err(|error| storage_error("FILE_NOT_FOUND", error, false))
}

fn document_node_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<DocumentNode> {
    let node_id: String = row.get(0)?;
    let parent_id: Option<String> = row.get(1)?;
    let table_json: Option<String> = row.get(5)?;
    let locator_json: String = row.get(6)?;
    let heading_path_json: String = row.get(7)?;
    Ok(DocumentNode {
        node_id: parse_uuid_column(&node_id, 0)?,
        parent_id: parent_id
            .map(|value| parse_uuid_column(&value, 1))
            .transpose()?,
        ordinal: row.get(2)?,
        node_type: row.get(3)?,
        text: row.get(4)?,
        table_data: table_json
            .as_deref()
            .map(|value| parse_json_column(5, value))
            .transpose()?,
        locator: parse_json_column(6, &locator_json)?,
        heading_path: parse_json_column(7, &heading_path_json)?,
    })
}

fn parse_json_column<T: serde::de::DeserializeOwned>(
    index: usize,
    value: &str,
) -> rusqlite::Result<T> {
    serde_json::from_str(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
}

fn file_matches_scope(
    connection: &Connection,
    file: &FileRecord,
    scope: &ScopeFilter,
) -> Result<bool, AppError> {
    if !file_is_authorized(connection, &file.file_id)? {
        return Ok(false);
    }
    if !scope.collection_ids.is_empty()
        && !file_matches_collection_scope(connection, file, &scope.collection_ids)?
    {
        return Ok(false);
    }
    if !scope.file_ids.is_empty() && !scope.file_ids.contains(&file.file_id) {
        return Ok(false);
    }
    if !scope.extensions.is_empty()
        && !scope.extensions.iter().any(|extension| {
            extension
                .trim_start_matches('.')
                .eq_ignore_ascii_case(&file.extension)
        })
    {
        return Ok(false);
    }
    if file.availability != scope.availability {
        return Ok(false);
    }
    if scope
        .modified_from
        .is_some_and(|from| file.fs_modified_at < from)
        || scope.modified_to.is_some_and(|to| file.fs_modified_at > to)
    {
        return Ok(false);
    }
    if scope.root_ids.is_empty() {
        return Ok(true);
    }
    let allowed = scope
        .root_ids
        .iter()
        .map(Uuid::to_string)
        .collect::<HashSet<_>>();
    let mut statement = connection
        .prepare("SELECT root_id FROM file_root_memberships WHERE file_id = ?1")
        .map_err(|error| storage_error("SEARCH_SCOPE_QUERY_FAILED", error, true))?;
    let roots = statement
        .query_map([file.file_id.to_string()], |row| row.get::<_, String>(0))
        .map_err(|error| storage_error("SEARCH_SCOPE_QUERY_FAILED", error, true))?;
    for root in roots {
        if allowed.contains(
            &root.map_err(|error| storage_error("SEARCH_SCOPE_QUERY_FAILED", error, true))?,
        ) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn collect_scoped_file_ids(
    connection: &Connection,
    files: &[FileRecord],
    scope: &ScopeFilter,
) -> Result<HashSet<Uuid>, AppError> {
    let requested_roots = scope
        .root_ids
        .iter()
        .map(Uuid::to_string)
        .collect::<HashSet<_>>();
    let mut authorized = HashSet::new();
    let mut statement = connection
        .prepare(
            "SELECT m.file_id, m.root_id FROM file_root_memberships m JOIN roots r ON r.root_id = m.root_id WHERE r.enabled = 1",
        )
        .map_err(|error| storage_error("SEARCH_SCOPE_QUERY_FAILED", error, true))?;
    let memberships = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|error| storage_error("SEARCH_SCOPE_QUERY_FAILED", error, true))?;
    for membership in memberships {
        let (file_id, root_id) =
            membership.map_err(|error| storage_error("SEARCH_SCOPE_QUERY_FAILED", error, true))?;
        if (requested_roots.is_empty() || requested_roots.contains(&root_id))
            && let Ok(file_id) = Uuid::parse_str(&file_id)
        {
            authorized.insert(file_id);
        }
    }

    let mut scoped = HashSet::new();
    for file in files {
        if !authorized.contains(&file.file_id)
            || (!scope.file_ids.is_empty() && !scope.file_ids.contains(&file.file_id))
            || (!scope.extensions.is_empty()
                && !scope.extensions.iter().any(|extension| {
                    extension
                        .trim_start_matches('.')
                        .eq_ignore_ascii_case(&file.extension)
                }))
            || file.availability != scope.availability
            || scope
                .modified_from
                .is_some_and(|from| file.fs_modified_at < from)
            || scope.modified_to.is_some_and(|to| file.fs_modified_at > to)
        {
            continue;
        }
        if !scope.collection_ids.is_empty()
            && !file_matches_collection_scope(connection, file, &scope.collection_ids)?
        {
            continue;
        }
        scoped.insert(file.file_id);
    }
    Ok(scoped)
}

fn search_cursor_fingerprint(
    connection: &Connection,
    request: &crate::SearchRequest,
    semantic_model_id: Option<&str>,
) -> Result<String, AppError> {
    let snapshot = connection
        .query_row(
            "SELECT COUNT(*), COALESCE(MAX(last_seen_at), ''), (SELECT COUNT(*) FROM chunks), (SELECT COUNT(*) FROM chunk_embeddings) FROM files",
            [],
            |row| {
                Ok((
                    row.get::<_, u64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, u64>(2)?,
                    row.get::<_, u64>(3)?,
                ))
            },
        )
        .map_err(|error| storage_error("SEARCH_QUERY_FAILED", error, true))?;
    let payload = serde_json::to_vec(&serde_json::json!({
        "query": request.query.trim(),
        "scope": &request.scope,
        "mode": request.mode,
        "sort": request.sort,
        "page_size": request.page_size,
        "semantic_model_id": semantic_model_id,
        "index_snapshot": snapshot,
    }))
    .map_err(|error| AppError::new("SEARCH_CURSOR_INVALID", error.to_string(), false))?;
    let mut digest = Sha256::new();
    digest.update(payload);
    Ok(format!("{:x}", digest.finalize()))
}

fn file_matches_collection_scope(
    connection: &Connection,
    file: &FileRecord,
    collection_ids: &[Uuid],
) -> Result<bool, AppError> {
    for collection_id in collection_ids {
        let raw = connection
            .query_row(
                "SELECT kind, rule_json FROM collections WHERE collection_id = ?1",
                [collection_id.to_string()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .optional()
            .map_err(|error| storage_error("COLLECTION_QUERY_FAILED", error, true))?;
        let Some((kind, rule_json)) = raw else {
            continue;
        };
        if kind == "manual" {
            let member = connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM collection_memberships WHERE collection_id = ?1 AND file_id = ?2)",
                    params![collection_id.to_string(), file.file_id.to_string()],
                    |row| row.get::<_, i64>(0),
                )
                .map_err(|error| storage_error("COLLECTION_QUERY_FAILED", error, true))?
                != 0;
            if member {
                return Ok(true);
            }
        } else if let Some(rule_json) = rule_json {
            let rule = serde_json::from_str::<CollectionRule>(&rule_json).map_err(|error| {
                AppError::new("COLLECTION_RULE_INVALID", error.to_string(), false)
            })?;
            if collection_rule_matches(connection, &rule, file)? {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn search_fulltext(
    connection: &Connection,
    query: &str,
    scope: &ScopeFilter,
) -> Result<Vec<RankedHit>, AppError> {
    let match_query = fts_query(query);
    if match_query.is_empty() {
        return Ok(Vec::new());
    }
    let mut statement = connection
        .prepare(
            "SELECT f.file_id, f.volume_id, f.canonical_path, f.display_name, f.extension, f.mime_type, f.size_bytes, f.fs_created_at, f.modified_at, f.windows_file_id, f.content_sha256, f.availability, f.current_revision_id, f.parse_status, f.first_seen_at, f.last_seen_at, c.revision_id, c.text, c.locator_json, bm25(chunks_fts) FROM chunks_fts JOIN chunks c ON c.chunk_id = chunks_fts.chunk_id JOIN files f ON f.file_id = c.file_id WHERE chunks_fts MATCH ?1 AND f.current_revision_id = c.revision_id ORDER BY bm25(chunks_fts) LIMIT 500",
        )
        .map_err(|error| storage_error("SEARCH_QUERY_FAILED", error, true))?;
    let mapped = statement
        .query_map([match_query], |row| {
            let file = file_from_row(row)?;
            let revision_id: String = row.get(16)?;
            let locator_json: String = row.get(18)?;
            let rank: f64 = row.get(19)?;
            let locator =
                serde_json::from_str::<SourceLocator>(&locator_json).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        18,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?;
            Ok((file, revision_id, row.get::<_, String>(17)?, locator, rank))
        })
        .map_err(|error| storage_error("SEARCH_QUERY_FAILED", error, true))?;
    let mut best_by_file: std::collections::HashMap<Uuid, RankedHit> =
        std::collections::HashMap::new();
    for row in mapped {
        let (file, revision_id, text, locator, rank) =
            row.map_err(|error| storage_error("SEARCH_QUERY_FAILED", error, true))?;
        if !file_matches_scope(connection, &file, scope)? {
            continue;
        }
        let score = (1.0 / (1.0 + rank.abs())) as f32;
        let hit = RankedHit {
            file: file.clone(),
            revision_id: Some(parse_uuid_value(&revision_id)?),
            snippet: text,
            locator: Some(locator),
            reason: "fulltext",
            channel_score: score,
        };
        let should_replace = best_by_file
            .get(&file.file_id)
            .is_none_or(|current| hit.channel_score > current.channel_score);
        if should_replace {
            best_by_file.insert(file.file_id, hit);
        }
    }
    Ok(best_by_file.into_values().collect())
}

fn search_semantic(
    connection: &Connection,
    query: &SemanticQuery<'_>,
    scoped_file_ids: &HashSet<Uuid>,
) -> Result<Vec<RankedHit>, AppError> {
    if query.model_artifact_id.trim().is_empty()
        || query.vector.is_empty()
        || query.vector.iter().any(|value| !value.is_finite())
    {
        return Err(AppError::new(
            "EMBEDDING_QUERY_INVALID",
            "语义查询向量无效",
            false,
        ));
    }
    let mut statement = connection
        .prepare(
            "SELECT e.chunk_id, e.file_id, e.dimension, e.vector_blob FROM chunk_embeddings e JOIN files f ON f.file_id = e.file_id WHERE e.model_artifact_id = ?1 AND f.current_revision_id = e.revision_id AND f.availability = 'present'",
        )
        .map_err(|error| storage_error("EMBEDDING_QUERY_FAILED", error, true))?;
    let mapped = statement
        .query_map([query.model_artifact_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, u32>(2)?,
                row.get::<_, Vec<u8>>(3)?,
            ))
        })
        .map_err(|error| storage_error("EMBEDDING_QUERY_FAILED", error, true))?;
    let mut best_by_file = std::collections::HashMap::<Uuid, (String, f32)>::new();
    for row in mapped {
        let (chunk_id, file_id, dimension, vector_blob) =
            row.map_err(|error| storage_error("EMBEDDING_QUERY_FAILED", error, true))?;
        let file_id = parse_uuid_value(&file_id)?;
        if dimension as usize != query.vector.len() || !scoped_file_ids.contains(&file_id) {
            continue;
        }
        let similarity = dot_product_with_le_f32(query.vector, &vector_blob, dimension)?;
        let score = ((similarity + 1.0) / 2.0).clamp(0.0, 1.0);
        if score < 0.2 {
            continue;
        }
        if best_by_file
            .get(&file_id)
            .is_none_or(|current| score > current.1)
        {
            best_by_file.insert(file_id, (chunk_id, score));
        }
    }
    let mut candidates = best_by_file.into_iter().collect::<Vec<_>>();
    candidates.sort_by(|left, right| right.1.1.total_cmp(&left.1.1));
    candidates.truncate(500);

    let mut details = connection
        .prepare(
            "SELECT f.file_id, f.volume_id, f.canonical_path, f.display_name, f.extension, f.mime_type, f.size_bytes, f.fs_created_at, f.modified_at, f.windows_file_id, f.content_sha256, f.availability, f.current_revision_id, f.parse_status, f.first_seen_at, f.last_seen_at, c.revision_id, c.text, c.locator_json FROM chunks c JOIN files f ON f.file_id = c.file_id WHERE c.chunk_id = ?1 AND f.current_revision_id = c.revision_id",
        )
        .map_err(|error| storage_error("EMBEDDING_QUERY_FAILED", error, true))?;
    let mut hits = Vec::with_capacity(candidates.len());
    for (_file_id, (chunk_id, score)) in candidates {
        let detail = details
            .query_row([chunk_id], |row| {
                let file = file_from_row(row)?;
                Ok((
                    file,
                    row.get::<_, String>(16)?,
                    row.get::<_, String>(17)?,
                    row.get::<_, String>(18)?,
                ))
            })
            .optional()
            .map_err(|error| storage_error("EMBEDDING_QUERY_FAILED", error, true))?;
        let Some((file, revision_id, text, locator_json)) = detail else {
            continue;
        };
        let locator = serde_json::from_str::<SourceLocator>(&locator_json)
            .map_err(|error| AppError::new("EMBEDDING_VECTOR_INVALID", error.to_string(), false))?;
        hits.push(RankedHit {
            file,
            revision_id: Some(parse_uuid_value(&revision_id)?),
            snippet: text,
            locator: Some(locator),
            reason: "semantic",
            channel_score: score,
        });
    }
    Ok(hits)
}

fn dot_product_with_le_f32(query: &[f32], bytes: &[u8], dimension: u32) -> Result<f32, AppError> {
    if bytes.len() != dimension as usize * std::mem::size_of::<f32>() {
        return Err(AppError::new(
            "EMBEDDING_VECTOR_INVALID",
            "持久化向量字节长度与维度不一致",
            false,
        ));
    }
    let mut product = 0.0_f32;
    for (left, chunk) in query.iter().zip(bytes.chunks_exact(4)) {
        let right = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        if !right.is_finite() {
            return Err(AppError::new(
                "EMBEDDING_VECTOR_INVALID",
                "持久化向量包含无效数值",
                false,
            ));
        }
        product += left * right;
    }
    Ok(product)
}

// 精确检索先扫描紧凑向量，再只回读最佳命中的正文与定位；ANN不是V1正确性的依赖。

fn encode_vector(vector: &[f32]) -> Vec<u8> {
    vector
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

fn active_scan_job_with(
    connection: &Connection,
    root_id: &Uuid,
) -> Result<Option<JobRecord>, AppError> {
    connection
        .query_row(
            "SELECT job_id, job_type, status, stage, progress, processed_items, total_items, error_json, created_at, started_at, finished_at FROM jobs WHERE root_id = ?1 AND job_type = 'initial_scan' AND status IN ('queued', 'running') ORDER BY created_at DESC LIMIT 1",
            [root_id.to_string()],
            job_from_row,
        )
        .optional()
        .map_err(|error| storage_error("JOB_QUERY_FAILED", error, true))
}

fn upsert_file(
    transaction: &Transaction<'_>,
    root_id: &Uuid,
    file: &DiscoveredFile,
) -> Result<Uuid, AppError> {
    let stable_existing: Option<(String, Option<String>)> = if let Some(windows_file_id) =
        &file.windows_file_id
    {
        transaction
            .query_row(
                "SELECT file_id, windows_file_id FROM files WHERE volume_id = ?1 AND windows_file_id = ?2",
                params![file.volume_id, windows_file_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|error| storage_error("FILE_QUERY_FAILED", error, true))?
    } else {
        None
    };
    let path_existing: Option<(String, Option<String>)> = transaction
        .query_row(
            "SELECT file_id, windows_file_id FROM files WHERE path_key = ?1",
            [&file.path_key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|error| storage_error("FILE_QUERY_FAILED", error, true))?;

    let existing_id = if let Some((file_id, _)) = stable_existing {
        Some(file_id)
    } else if let Some((file_id, stored_windows_file_id)) = path_existing {
        if stored_windows_file_id.is_some()
            && file.windows_file_id.is_some()
            && stored_windows_file_id != file.windows_file_id
        {
            transaction
                .execute(
                    "UPDATE files SET path_key = path_key || '#replaced-' || file_id, availability = 'missing' WHERE file_id = ?1",
                    [&file_id],
                )
                .map_err(|error| storage_error("FILE_UPSERT_FAILED", error, true))?;
            None
        } else {
            Some(file_id)
        }
    } else {
        None
    };
    let is_new = existing_id.is_none();
    let file_id = existing_id
        .as_deref()
        .map(parse_uuid_value)
        .transpose()?
        .unwrap_or_else(Uuid::now_v7);
    let previous_state: Option<(String, String)> = transaction
        .query_row(
            "SELECT canonical_path, availability FROM files WHERE file_id = ?1",
            [file_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|error| storage_error("FILE_QUERY_FAILED", error, true))?;
    let now = Utc::now();
    let fingerprint = format!("{}:{}", file.size_bytes, file.modified_at.to_rfc3339());
    let existing_fingerprint: Option<String> = transaction
        .query_row(
            "SELECT r.metadata_fingerprint FROM files f LEFT JOIN file_revisions r ON r.revision_id = f.current_revision_id WHERE f.file_id = ?1",
            [file_id.to_string()],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| storage_error("REVISION_QUERY_FAILED", error, true))?
        .flatten();
    let revision_changed = existing_fingerprint.as_deref() != Some(&fingerprint);
    let revision_id = if revision_changed {
        transaction
            .execute(
                "INSERT OR IGNORE INTO files (file_id, canonical_path, path_key, name, extension, size_bytes, modified_at, discovered_at, availability, volume_id, display_name, mime_type, fs_created_at, windows_file_id, parse_status, first_seen_at, last_seen_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'present', ?9, ?4, ?10, ?11, ?12, 'pending', ?8, ?8)",
                params![file_id.to_string(), file.canonical_path, file.path_key, file.name, file.extension, file.size_bytes, file.modified_at.to_rfc3339(), now.to_rfc3339(), file.volume_id, file.mime_type, file.created_at.map(|value| value.to_rfc3339()), file.windows_file_id],
            )
            .map_err(|error| storage_error("FILE_UPSERT_FAILED", error, true))?;
        let revision_id = Uuid::now_v7();
        transaction
            .execute(
                "INSERT INTO file_revisions (revision_id, file_id, size_bytes, fs_modified_at, content_sha256, metadata_fingerprint, created_at) VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6)",
                params![revision_id.to_string(), file_id.to_string(), file.size_bytes, file.modified_at.to_rfc3339(), fingerprint, now.to_rfc3339()],
            )
            .map_err(|error| storage_error("REVISION_INSERT_FAILED", error, true))?;
        revision_id
    } else {
        transaction
            .query_row(
                "SELECT current_revision_id FROM files WHERE file_id = ?1",
                [file_id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .map_err(|error| storage_error("REVISION_QUERY_FAILED", error, true))
            .and_then(|value| parse_uuid_value(&value))?
    };
    transaction
        .execute(
            "INSERT INTO files (file_id, canonical_path, path_key, name, extension, size_bytes, modified_at, discovered_at, availability, volume_id, display_name, mime_type, fs_created_at, windows_file_id, current_revision_id, parse_status, first_seen_at, last_seen_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'present', ?9, ?4, ?10, ?11, ?12, ?13, 'pending', ?8, ?8) ON CONFLICT(file_id) DO UPDATE SET canonical_path = excluded.canonical_path, path_key = excluded.path_key, name = excluded.name, display_name = excluded.display_name, extension = excluded.extension, mime_type = excluded.mime_type, size_bytes = excluded.size_bytes, fs_created_at = excluded.fs_created_at, modified_at = excluded.modified_at, windows_file_id = excluded.windows_file_id, volume_id = excluded.volume_id, content_sha256 = CASE WHEN files.current_revision_id <> excluded.current_revision_id THEN NULL ELSE files.content_sha256 END, current_revision_id = excluded.current_revision_id, parse_status = CASE WHEN files.current_revision_id <> excluded.current_revision_id THEN 'pending' ELSE files.parse_status END, availability = 'present', last_seen_at = excluded.last_seen_at",
            params![
                file_id.to_string(),
                file.canonical_path,
                file.path_key,
                file.name,
                file.extension,
                file.size_bytes,
                file.modified_at.to_rfc3339(),
                now.to_rfc3339(),
                file.volume_id,
                file.mime_type,
                file.created_at.map(|value| value.to_rfc3339()),
                file.windows_file_id,
                revision_id.to_string(),
            ],
        )
        .map_err(|error| storage_error("FILE_UPSERT_FAILED", error, true))?;
    transaction
        .execute(
            "INSERT INTO file_root_memberships (file_id, root_id, relative_path, is_primary) VALUES (?1, ?2, ?3, CASE WHEN EXISTS(SELECT 1 FROM file_root_memberships WHERE file_id = ?1) THEN 0 ELSE 1 END) ON CONFLICT(file_id, root_id) DO UPDATE SET relative_path = excluded.relative_path",
            params![file_id.to_string(), root_id.to_string(), file.relative_path],
        )
        .map_err(|error| storage_error("MEMBERSHIP_UPSERT_FAILED", error, true))?;
    let renamed_from = previous_state
        .as_ref()
        .map(|(path, _)| path.as_str())
        .filter(|path| *path != file.canonical_path);
    let restored = previous_state
        .as_ref()
        .is_some_and(|(_, availability)| availability == "missing");
    if is_new || revision_changed || renamed_from.is_some() || restored {
        let event_type = if is_new {
            InboxEventType::Discovered
        } else if renamed_from.is_some() {
            InboxEventType::Renamed
        } else if restored {
            InboxEventType::Restored
        } else {
            InboxEventType::Modified
        };
        let summary = match event_type {
            InboxEventType::Discovered => format!("已发现资料：{}", file.name),
            InboxEventType::Renamed => format!("资料已重命名：{}", file.name),
            InboxEventType::Restored => format!("资料已恢复：{}", file.name),
            InboxEventType::Modified => format!("资料有新版本：{}", file.name),
            _ => format!("资料状态变化：{}", file.name),
        };
        insert_inbox_event(
            transaction,
            &file_id,
            event_type,
            now,
            renamed_from,
            TriageStatus::New,
            Some(&summary),
            None,
            &format!(
                "scan:{}:{}:{}",
                file_id,
                revision_id,
                event_type.as_storage()
            ),
        )?;
    }
    Ok(file_id)
}

fn reconcile_root_memberships(
    transaction: &Transaction<'_>,
    root_id: &Uuid,
    seen_file_ids: &[Uuid],
) -> Result<(), AppError> {
    let mut statement = transaction
        .prepare("SELECT file_id FROM file_root_memberships WHERE root_id = ?1")
        .map_err(|error| storage_error("MEMBERSHIP_QUERY_FAILED", error, true))?;
    let existing = statement
        .query_map([root_id.to_string()], |row| row.get::<_, String>(0))
        .map_err(|error| storage_error("MEMBERSHIP_QUERY_FAILED", error, true))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| storage_error("MEMBERSHIP_QUERY_FAILED", error, true))?;
    drop(statement);
    let seen = seen_file_ids
        .iter()
        .map(Uuid::to_string)
        .collect::<std::collections::HashSet<_>>();
    for file_id in existing
        .into_iter()
        .filter(|file_id| !seen.contains(file_id))
    {
        transaction
            .execute(
                "DELETE FROM file_root_memberships WHERE file_id = ?1 AND root_id = ?2",
                params![file_id, root_id.to_string()],
            )
            .map_err(|error| storage_error("MEMBERSHIP_DELETE_FAILED", error, true))?;
        let changed = transaction
            .execute(
                "UPDATE files SET availability = 'missing' WHERE file_id = ?1 AND availability <> 'missing' AND NOT EXISTS(SELECT 1 FROM file_root_memberships WHERE file_id = ?1)",
                [&file_id],
            )
            .map_err(|error| storage_error("FILE_UPSERT_FAILED", error, true))?;
        if changed != 0 {
            let parsed_file_id = parse_uuid_value(&file_id)?;
            let display_name: String = transaction
                .query_row(
                    "SELECT display_name FROM files WHERE file_id = ?1",
                    [&file_id],
                    |row| row.get(0),
                )
                .map_err(|error| storage_error("FILE_QUERY_FAILED", error, true))?;
            let observed_at = Utc::now();
            insert_inbox_event(
                transaction,
                &parsed_file_id,
                InboxEventType::Missing,
                observed_at,
                None,
                TriageStatus::New,
                Some(&format!("资料已不在原位置：{display_name}")),
                None,
                &format!("missing:{file_id}:{}", observed_at.timestamp_millis()),
            )?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn insert_inbox_event(
    transaction: &Transaction<'_>,
    file_id: &Uuid,
    event_type: InboxEventType,
    observed_at: DateTime<Utc>,
    previous_path: Option<&str>,
    triage_status: TriageStatus,
    summary: Option<&str>,
    error_code: Option<&str>,
    dedupe_key: &str,
) -> Result<(), AppError> {
    transaction
        .execute(
            "INSERT INTO inbox_events (inbox_id, dedupe_key, file_id, event_type, observed_at, previous_path, triage_status, summary, error_code) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) ON CONFLICT(dedupe_key) DO UPDATE SET observed_at = excluded.observed_at, triage_status = excluded.triage_status, summary = excluded.summary, error_code = excluded.error_code",
            params![
                Uuid::now_v7().to_string(),
                dedupe_key,
                file_id.to_string(),
                event_type.as_storage(),
                observed_at.to_rfc3339(),
                previous_path,
                triage_status.as_storage(),
                summary,
                error_code,
            ],
        )
        .map_err(|error| storage_error("INBOX_WRITE_FAILED", error, true))?;
    Ok(())
}

fn candidate_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CandidateRoot> {
    let candidate_id: String = row.get(0)?;
    let candidate_type: String = row.get(1)?;
    let status: String = row.get(3)?;
    let candidate_type = CandidateRootType::from_storage(&candidate_type);
    Ok(CandidateRoot {
        candidate_id: parse_uuid_column(&candidate_id, 0)?,
        candidate_type,
        label: candidate_type.label().to_owned(),
        display_path: row.get(2)?,
        status: CandidateRootStatus::from_storage(&status),
    })
}

fn refresh_root_coverage(connection: &Connection) -> Result<(), AppError> {
    let roots = {
        let mut statement = connection
            .prepare("SELECT root_id, path_key FROM roots WHERE enabled = 1")
            .map_err(|error| storage_error("ROOT_QUERY_FAILED", error, true))?;
        statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|error| storage_error("ROOT_QUERY_FAILED", error, true))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| storage_error("ROOT_QUERY_FAILED", error, true))?
    };
    for (root_id, root_path_key) in &roots {
        let parent = roots
            .iter()
            .filter(|(candidate_id, candidate_path_key)| {
                candidate_id != root_id
                    && root_path_key.starts_with(&format!("{candidate_path_key}\\"))
            })
            .max_by_key(|(_, candidate_path_key)| candidate_path_key.len())
            .map(|(candidate_id, _)| candidate_id);
        connection
            .execute(
                "UPDATE roots SET coverage_parent_root_id = ?1 WHERE root_id = ?2",
                params![parent, root_id],
            )
            .map_err(|error| storage_error("ROOT_UPDATE_FAILED", error, true))?;
    }
    Ok(())
}

fn root_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RootRecord> {
    let root_id: String = row.get(0)?;
    let volume_type: String = row.get(6)?;
    let authorization_source: String = row.get(7)?;
    let root_kind: String = row.get(8)?;
    let status: String = row.get(11)?;
    let watch_mode: String = row.get(12)?;
    let coverage_parent_root_id: Option<String> = row.get(13)?;
    let last_scan_at: Option<String> = row.get(16)?;
    Ok(RootRecord {
        root_id: Uuid::parse_str(&root_id).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?,
        path: row.get(1)?,
        canonical_path: row.get(2)?,
        path_key: row.get(3)?,
        root_file_id: row.get(4)?,
        volume_id: row.get(5)?,
        volume_type: VolumeType::from_storage(&volume_type),
        authorization_source: AuthorizationSource::from_storage(&authorization_source),
        root_kind: RootKind::from_storage(&root_kind),
        label: row.get(9)?,
        enabled: row.get::<_, i64>(10)? != 0,
        status: RootStatus::from_storage(&status),
        watch_mode: WatchMode::from_storage(&watch_mode),
        coverage_parent_root_id: coverage_parent_root_id
            .map(|value| parse_uuid_column(&value, 13))
            .transpose()?,
        file_count: row.get(14)?,
        permission_error_count: row.get(15)?,
        last_scan_at: last_scan_at
            .and_then(|value| DateTime::parse_from_rfc3339(&value).ok())
            .map(|value| value.with_timezone(&Utc)),
    })
}

fn file_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<FileRecord> {
    let file_id: String = row.get(0)?;
    let fs_created_at: Option<String> = row.get(7)?;
    let fs_modified_at: String = row.get(8)?;
    let availability: String = row.get(11)?;
    let current_revision_id: Option<String> = row.get(12)?;
    let parse_status: String = row.get(13)?;
    let first_seen_at: String = row.get(14)?;
    let last_seen_at: String = row.get(15)?;
    Ok(FileRecord {
        file_id: parse_uuid_column(&file_id, 0)?,
        volume_id: row.get(1)?,
        canonical_path: row.get(2)?,
        display_name: row.get(3)?,
        extension: row.get(4)?,
        mime_type: row.get(5)?,
        size_bytes: row.get(6)?,
        fs_created_at: fs_created_at
            .map(|value| parse_datetime_column(&value, 7))
            .transpose()?,
        fs_modified_at: parse_datetime_column(&fs_modified_at, 8)?,
        windows_file_id: row.get(9)?,
        content_sha256: row.get(10)?,
        availability: crate::Availability::from_storage(&availability),
        current_revision_id: current_revision_id
            .map(|value| parse_uuid_column(&value, 12))
            .transpose()?,
        parse_status: ParseStatus::from_storage(&parse_status),
        first_seen_at: parse_datetime_column(&first_seen_at, 14)?,
        last_seen_at: parse_datetime_column(&last_seen_at, 15)?,
    })
}

fn job_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<JobRecord> {
    let job_id: String = row.get(0)?;
    let status: String = row.get(2)?;
    let error_json: Option<String> = row.get(7)?;
    let created_at: String = row.get(8)?;
    let started_at: Option<String> = row.get(9)?;
    let finished_at: Option<String> = row.get(10)?;
    Ok(JobRecord {
        job_id: parse_uuid_column(&job_id, 0)?,
        job_type: row.get(1)?,
        status: JobStatus::from_storage(&status),
        stage: row.get(3)?,
        progress: row.get(4)?,
        processed_items: row.get(5)?,
        total_items: row.get(6)?,
        error: error_json
            .map(|value| serde_json::from_str(&value))
            .transpose()
            .map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    7,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })?,
        created_at: parse_datetime_column(&created_at, 8)?,
        started_at: started_at
            .map(|value| parse_datetime_column(&value, 9))
            .transpose()?,
        finished_at: finished_at
            .map(|value| parse_datetime_column(&value, 10))
            .transpose()?,
    })
}

fn parse_uuid_column(value: &str, index: usize) -> rusqlite::Result<Uuid> {
    Uuid::parse_str(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
}

fn parse_uuid_value(value: &str) -> Result<Uuid, AppError> {
    Uuid::parse_str(value)
        .map_err(|error| AppError::new("SCHEMA_UUID_V7_REQUIRED", error.to_string(), false))
}

fn parse_datetime_value(value: &str) -> Result<DateTime<Utc>, AppError> {
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|error| AppError::new("TASK_CHECKPOINT_DATA_INVALID", error.to_string(), false))
}

fn parse_datetime_column(value: &str, index: usize) -> rusqlite::Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                index,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })
}

fn storage_error(code: &str, error: rusqlite::Error, retryable: bool) -> AppError {
    AppError::new(code, error.to_string(), retryable)
}

fn checkpoint_type_as_str(value: CheckpointType) -> &'static str {
    match value {
        CheckpointType::Schema => "schema",
        CheckpointType::Invariant => "invariant",
        CheckpointType::Evidence => "evidence",
        CheckpointType::Permission => "permission",
        CheckpointType::Resource => "resource",
        CheckpointType::Quality => "quality",
    }
}

fn checkpoint_type_from_str(value: &str) -> Result<CheckpointType, AppError> {
    match value {
        "schema" => Ok(CheckpointType::Schema),
        "invariant" => Ok(CheckpointType::Invariant),
        "evidence" => Ok(CheckpointType::Evidence),
        "permission" => Ok(CheckpointType::Permission),
        "resource" => Ok(CheckpointType::Resource),
        "quality" => Ok(CheckpointType::Quality),
        _ => Err(AppError::new(
            "TASK_CHECKPOINT_DATA_INVALID",
            "持久化检查点类型无效",
            false,
        )),
    }
}

fn checkpoint_status_from_str(value: &str) -> Result<CheckpointStatus, AppError> {
    match value {
        "passed" => Ok(CheckpointStatus::Passed),
        "failed" => Ok(CheckpointStatus::Failed),
        "warning" => Ok(CheckpointStatus::Warning),
        _ => Err(AppError::new(
            "TASK_CHECKPOINT_DATA_INVALID",
            "持久化检查点状态无效",
            false,
        )),
    }
}

impl JobStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Paused => "paused",
            Self::AwaitingUser => "awaiting_user",
            Self::Succeeded => "succeeded",
            Self::Partial => "partial",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn from_storage(value: &str) -> Self {
        match value {
            "running" => Self::Running,
            "paused" => Self::Paused,
            "awaiting_user" => Self::AwaitingUser,
            "succeeded" => Self::Succeeded,
            "partial" => Self::Partial,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            _ => Self::Queued,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_root_registration() -> RootRegistration {
        RootRegistration {
            label: "文档".to_owned(),
            canonical_path: "C:\\Users\\Test\\Documents".to_owned(),
            path_key: "c:\\users\\test\\documents".to_owned(),
            source: RootSource::KnownFolder,
            volume_id: "vol-test".to_owned(),
            root_file_id: Some("root-file-test".to_owned()),
            authorization_source: AuthorizationSource::SystemDefault,
            root_kind: RootKind::KnownFolder,
            volume_type: VolumeType::Fixed,
            watch_mode: WatchMode::Realtime,
        }
    }

    #[test]
    fn root_registration_is_idempotent() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = CatalogStore::open(directory.path().join("remin.db")).expect("open store");
        let first = store
            .upsert_root(&test_root_registration())
            .expect("insert root");
        let second = store
            .upsert_root(&test_root_registration())
            .expect("reuse root");

        assert_eq!(first.root_id, second.root_id);
        assert_eq!(store.list_roots().expect("list roots").len(), 1);
    }

    #[test]
    fn disabled_default_root_stays_disabled_until_user_adds_it_again() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = CatalogStore::open(directory.path().join("remin.db")).expect("open store");
        let root = store
            .upsert_root(&test_root_registration())
            .expect("insert root");
        store.disable_root(&root.root_id).expect("disable root");

        let rediscovered = store
            .upsert_root(&test_root_registration())
            .expect("keep user override");
        assert!(!rediscovered.enabled);
        assert!(store.list_roots().expect("list roots").is_empty());

        let mut selected = test_root_registration();
        selected.source = RootSource::UserFolder;
        selected.authorization_source = AuthorizationSource::UserSelected;
        selected.root_kind = RootKind::Folder;
        let reenabled = store
            .upsert_root(&selected)
            .expect("explicitly reenable root");
        assert!(reenabled.enabled);
        assert_eq!(reenabled.root_id, root.root_id);
    }

    #[test]
    fn task_steps_and_checkpoints_are_persisted_before_completion() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = CatalogStore::open(directory.path().join("remin.db")).expect("open store");
        let plan = crate::plan_skill(&crate::PlanSkillRequest {
            task_id: None,
            skill_id: "generate_catalog".into(),
            file_ids: vec![Uuid::now_v7()],
            parameters: serde_json::json!({"preset_id": "file_catalog"}),
            user_instruction: None,
        })
        .expect("plan task");
        let job = store.begin_task(&plan).expect("begin task");
        assert_eq!(job.status, JobStatus::Running);

        let checkpoint_types = [
            CheckpointType::Permission,
            CheckpointType::Invariant,
            CheckpointType::Evidence,
            CheckpointType::Schema,
        ];
        for (step, checkpoint_type) in plan.steps.iter().zip(checkpoint_types) {
            let checkpoint = store
                .pass_task_step(
                    &plan.task_id,
                    step,
                    checkpoint_type,
                    serde_json::json!({"verified": true}),
                )
                .expect("pass task step");
            assert_eq!(checkpoint.status, CheckpointStatus::Passed);
        }
        let completed = store.finish_task(&plan.task_id).expect("finish task");
        assert_eq!(completed.status, JobStatus::Succeeded);
        assert_eq!(completed.progress, 1.0);
        assert_eq!(completed.processed_items, completed.total_items);
    }

    #[test]
    fn failed_task_resumes_from_persisted_checkpoint() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = CatalogStore::open(directory.path().join("remin.db")).expect("open store");
        let plan = crate::plan_skill(&crate::PlanSkillRequest {
            task_id: None,
            skill_id: "generate_catalog".into(),
            file_ids: vec![Uuid::now_v7()],
            parameters: serde_json::json!({"preset_id": "file_catalog"}),
            user_instruction: None,
        })
        .expect("plan task");
        store.begin_task(&plan).expect("begin task");
        let candidates = [
            ("content_heading", crate::CandidateStatus::Selected),
            ("metadata_only", crate::CandidateStatus::Valid),
            ("conservative_fallback", crate::CandidateStatus::Valid),
        ]
        .into_iter()
        .map(|(strategy, status)| ExplorationCandidate {
            candidate_id: Uuid::now_v7(),
            job_id: plan.task_id,
            strategy: strategy.into(),
            status,
            result_ref: (status == crate::CandidateStatus::Selected)
                .then(|| "remin://extraction/test".into()),
            quality_score: Some(0.8),
            evidence_score: Some(1.0),
            latency_ms: Some(1),
            resource_cost: Some(0.1),
            rejection_reasons: Vec::new(),
        })
        .collect::<Vec<_>>();
        store
            .replace_task_exploration_candidates(&plan.task_id, &candidates)
            .expect("record exploration candidates");
        assert_eq!(
            store
                .task_exploration_candidates(&plan.task_id)
                .expect("list exploration candidates"),
            candidates
        );
        let first = store
            .pass_task_step(
                &plan.task_id,
                &plan.steps[0],
                CheckpointType::Permission,
                serde_json::json!({"verified": true}),
            )
            .expect("pass first checkpoint");
        assert!(first.resume_token.is_some());
        store
            .fail_task(
                &plan.task_id,
                &AppError::new("WORKER_IO_FAILED", "temporary failure", true),
            )
            .expect("fail task");

        let recovered_plan = store
            .latest_recoverable_task_plan()
            .expect("query recoverable plan")
            .expect("recoverable plan");
        assert_eq!(recovered_plan.task_id, plan.task_id);
        assert_eq!(recovered_plan.steps[0].step_id, plan.steps[0].step_id);
        assert_eq!(
            store
                .resume_task(&plan.task_id)
                .expect("resume task")
                .status,
            JobStatus::Running
        );
        for (step, kind) in plan.steps.iter().skip(1).zip([
            CheckpointType::Invariant,
            CheckpointType::Evidence,
            CheckpointType::Schema,
        ]) {
            store
                .pass_task_step(
                    &plan.task_id,
                    step,
                    kind,
                    serde_json::json!({"verified": true}),
                )
                .expect("pass resumed checkpoint");
        }
        let checkpoints = store
            .task_checkpoints(&plan.task_id)
            .expect("list checkpoints");
        assert_eq!(checkpoints.len(), 4);
        assert!(checkpoints.iter().all(|item| item.resume_token.is_some()));
        assert_eq!(
            store
                .finish_task(&plan.task_id)
                .expect("finish task")
                .status,
            JobStatus::Succeeded
        );
    }

    #[test]
    fn index_activity_stats_report_authorized_current_index_content() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = CatalogStore::open(directory.path().join("remin.db")).expect("open store");
        let root = store
            .upsert_root(&test_root_registration())
            .expect("insert root");
        let file_id = Uuid::now_v7();
        let revision_id = Uuid::now_v7();
        let first_node_id = Uuid::now_v7();
        let second_node_id = Uuid::now_v7();
        let third_node_id = Uuid::now_v7();
        let first_chunk_id = Uuid::now_v7();
        let second_chunk_id = Uuid::now_v7();
        let now = Utc::now().to_rfc3339();
        let connection = store.connect().expect("connect");
        connection
            .execute(
                "INSERT INTO files (file_id, canonical_path, path_key, name, extension, size_bytes, modified_at, discovered_at, availability, volume_id, display_name, mime_type, current_revision_id, parse_status, first_seen_at, last_seen_at) VALUES (?1, ?2, ?3, ?4, 'pdf', 256, ?5, ?5, 'present', 'vol-test', ?4, 'application/pdf', ?6, 'parsed', ?5, ?5)",
                params![file_id.to_string(), "C:\\Users\\Test\\Documents\\扫描资料.pdf", "c:\\users\\test\\documents\\扫描资料.pdf", "扫描资料.pdf", now, revision_id.to_string()],
            )
            .expect("insert file");
        connection
            .execute(
                "INSERT INTO file_revisions (revision_id, file_id, size_bytes, fs_modified_at, metadata_fingerprint, created_at, parse_status) VALUES (?1, ?2, 256, ?3, '256:ocr', ?3, 'parsed')",
                params![revision_id.to_string(), file_id.to_string(), now],
            )
            .expect("insert revision");
        connection
            .execute(
                "INSERT INTO file_root_memberships (file_id, root_id, relative_path, is_primary) VALUES (?1, ?2, '扫描资料.pdf', 1)",
                params![file_id.to_string(), root.root_id.to_string()],
            )
            .expect("insert membership");
        for (node_id, ordinal, page_no) in [
            (first_node_id, 0_u32, 1_u32),
            (second_node_id, 1, 1),
            (third_node_id, 2, 2),
        ] {
            connection
                .execute(
                    "INSERT INTO document_nodes (node_id, revision_id, ordinal, node_type, locator_json, heading_path_json, text) VALUES (?1, ?2, ?3, 'ocr_line', ?4, '[]', '识别文字')",
                    params![node_id.to_string(), revision_id.to_string(), ordinal, serde_json::json!({"page_no": page_no}).to_string()],
                )
                .expect("insert OCR node");
        }
        for (chunk_id, node_id, ordinal) in [
            (first_chunk_id, first_node_id, 0_u32),
            (second_chunk_id, third_node_id, 1_u32),
        ] {
            connection
                .execute(
                    "INSERT INTO chunks (chunk_id, file_id, revision_id, node_id, ordinal, text, normalized_text, token_count, content_hash, language, locator_json) VALUES (?1, ?2, ?3, ?4, ?5, '识别文字', '识 别 文 字', 4, ?1, 'zh', '{}')",
                    params![chunk_id.to_string(), file_id.to_string(), revision_id.to_string(), node_id.to_string(), ordinal],
                )
                .expect("insert chunk");
        }
        connection
            .execute(
                "INSERT INTO chunk_embeddings (chunk_id, model_artifact_id, file_id, revision_id, dimension, vector_blob, created_at) VALUES (?1, 'embedding-test', ?2, ?3, 2, X'00000000', ?4)",
                params![first_chunk_id.to_string(), file_id.to_string(), revision_id.to_string(), now],
            )
            .expect("insert embedding");
        drop(connection);

        assert_eq!(
            store.index_activity_stats().expect("index activity stats"),
            IndexActivityStats {
                discovered_files: 1,
                searchable_files: 1,
                parsed_files: 1,
                embedded_files: 1,
                ocr_pages: 2,
            }
        );
    }

    #[test]
    fn degradation_state_persists_and_changes_only_one_level_per_checkpoint() {
        let directory = tempfile::tempdir().expect("tempdir");
        let database_path = directory.path().join("remin.db");
        let store = CatalogStore::open(&database_path).expect("open store");

        let balanced = store
            .reconcile_degradation_state(
                DegradationLevel::Core,
                vec!["数据库健康检查异常".to_owned()],
            )
            .expect("degrade one level");
        assert_eq!(balanced.level, DegradationLevel::Balanced);
        assert!(
            balanced
                .disabled_features
                .contains(&"background_similarity".to_owned())
        );

        let core = store
            .reconcile_degradation_state(
                DegradationLevel::Core,
                vec!["数据库健康检查异常".to_owned()],
            )
            .expect("degrade to core");
        assert_eq!(core.level, DegradationLevel::Core);
        assert!(core.disabled_features.contains(&"generation".to_owned()));
        drop(store);

        let reopened = CatalogStore::open(&database_path).expect("reopen store");
        assert_eq!(
            reopened.degradation_state().expect("persisted state").level,
            DegradationLevel::Core
        );
        assert_eq!(
            reopened
                .reconcile_degradation_state(DegradationLevel::Full, Vec::new())
                .expect("respect recovery cooldown")
                .level,
            DegradationLevel::Core
        );
        let connection = reopened.connect().expect("connect for recovery");
        let first_recovery_at =
            core.recover_after.expect("core recovery time") + chrono::Duration::seconds(1);
        assert_eq!(
            CatalogStore::reconcile_degradation_state_at(
                &connection,
                DegradationLevel::Full,
                Vec::new(),
                first_recovery_at,
            )
            .expect("recover one level")
            .level,
            DegradationLevel::Balanced
        );
        let balanced_recovery = CatalogStore::degradation_state_with_connection(&connection)
            .expect("balanced state")
            .recover_after
            .expect("balanced recovery time")
            + chrono::Duration::seconds(1);
        assert_eq!(
            CatalogStore::reconcile_degradation_state_at(
                &connection,
                DegradationLevel::Full,
                Vec::new(),
                balanced_recovery,
            )
            .expect("recover fully")
            .level,
            DegradationLevel::Full
        );
    }

    #[test]
    fn overlapping_roots_record_the_nearest_coverage_parent() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = CatalogStore::open(directory.path().join("remin.db")).expect("open store");
        let parent = store
            .upsert_root(&test_root_registration())
            .expect("insert parent root");
        let child = store
            .upsert_root(&RootRegistration {
                label: "项目".to_owned(),
                canonical_path: "C:\\Users\\Test\\Documents\\Project".to_owned(),
                path_key: "c:\\users\\test\\documents\\project".to_owned(),
                source: RootSource::UserFolder,
                volume_id: "vol-test".to_owned(),
                root_file_id: Some("root-file-child".to_owned()),
                authorization_source: AuthorizationSource::UserSelected,
                root_kind: RootKind::Folder,
                volume_type: VolumeType::Fixed,
                watch_mode: WatchMode::Realtime,
            })
            .expect("insert child root");

        assert_eq!(child.coverage_parent_root_id, Some(parent.root_id));
    }

    #[test]
    fn fresh_database_applies_ordered_migration_history() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = CatalogStore::open(directory.path().join("remin.db")).expect("open store");

        assert_eq!(
            store.schema_version().expect("schema version"),
            CURRENT_SCHEMA_VERSION
        );
        assert_eq!(
            store.migration_history().expect("migration history"),
            vec![
                (1, "catalog_foundation".to_owned()),
                (2, "execution_contracts".to_owned()),
                (3, "stable_catalog_and_recovery".to_owned()),
                (4, "document_index_foundation".to_owned()),
                (5, "inbox_collections_and_relations".to_owned()),
                (6, "local_vector_index".to_owned()),
            ]
        );
        let rules = store.list_exclusion_rules().expect("list exclusion rules");
        assert_eq!(rules.len(), BUILT_IN_EXCLUSION_RULES.len());
        assert!(
            rules
                .iter()
                .filter(|rule| rule.rule_class == ExclusionRuleClass::Hard)
                .all(|rule| !rule.overridable)
        );
    }

    #[test]
    fn version_one_database_upgrades_without_reapplying_foundation() {
        let directory = tempfile::tempdir().expect("tempdir");
        let database_path = directory.path().join("remin.db");
        let connection = Connection::open(&database_path).expect("open legacy database");
        connection
            .execute_batch(MIGRATIONS[0].sql)
            .expect("create version one schema");
        connection
            .pragma_update(None, "user_version", 1)
            .expect("set legacy version");
        drop(connection);

        let store = CatalogStore::open(&database_path).expect("upgrade store");

        assert_eq!(
            store.schema_version().expect("schema version"),
            CURRENT_SCHEMA_VERSION
        );
        assert_eq!(store.migration_history().expect("history").len(), 6);
    }

    #[test]
    fn future_database_version_is_rejected_without_downgrade() {
        let directory = tempfile::tempdir().expect("tempdir");
        let database_path = directory.path().join("remin.db");
        let connection = Connection::open(&database_path).expect("open future database");
        connection
            .pragma_update(None, "user_version", CURRENT_SCHEMA_VERSION + 1)
            .expect("set future version");
        drop(connection);

        let error = CatalogStore::open(&database_path).expect_err("future schema must fail");

        assert_eq!(error.code, "DATABASE_SCHEMA_TOO_NEW");
        let connection = Connection::open(database_path).expect("reopen future database");
        let version: u32 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("read future version");
        assert_eq!(version, CURRENT_SCHEMA_VERSION + 1);
    }

    #[test]
    fn interrupted_scan_is_requeued_with_same_job_identity() {
        let directory = tempfile::tempdir().expect("tempdir");
        let database_path = directory.path().join("remin.db");
        let store = CatalogStore::open(&database_path).expect("open store");
        let root = store
            .upsert_root(&test_root_registration())
            .expect("insert root");
        let (job, created) = store
            .prepare_scan_job(&root.root_id, "recovery_test")
            .expect("prepare scan");
        assert!(created);
        store
            .mark_scan_running(&root.root_id, &job.job_id)
            .expect("mark running");
        drop(store);

        let reopened = CatalogStore::open(&database_path).expect("reopen store");
        let recovered = reopened
            .recover_interrupted_scan_jobs()
            .expect("recover jobs");

        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].0, root.root_id);
        assert_eq!(recovered[0].1.job_id, job.job_id);
        assert_eq!(recovered[0].1.status, JobStatus::Queued);
        assert_eq!(recovered[0].1.stage, "recovery_pending");
    }

    #[test]
    fn structured_logs_are_persisted_locally() {
        let directory = tempfile::tempdir().expect("tempdir");
        let database_path = directory.path().join("remin.db");
        let store = CatalogStore::open(&database_path).expect("open store");

        let fields = serde_json::json!({ "source": "unit_test" });
        store
            .append_log(&LogEventInput {
                level: "info",
                component: "test",
                event_name: "checkpoint.passed",
                job_id: None,
                root_id: None,
                file_id: None,
                fields: &fields,
            })
            .expect("append log");

        let connection = Connection::open(database_path).expect("open database");
        let count: u64 = connection
            .query_row("SELECT COUNT(*) FROM log_events", [], |row| row.get(0))
            .expect("count logs");
        assert_eq!(count, 1);
    }

    #[test]
    fn diagnostic_logs_are_returned_in_bounded_pages() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = CatalogStore::open(directory.path().join("remin.db")).expect("open store");
        for index in 0..3 {
            let fields = serde_json::json!({ "index": index });
            store
                .append_log(&LogEventInput {
                    level: "info",
                    component: "pagination_test",
                    event_name: "page.item",
                    job_id: None,
                    root_id: None,
                    file_id: None,
                    fields: &fields,
                })
                .expect("append log");
        }

        let first = store
            .query_logs(&LogQuery {
                cursor: None,
                page_size: 2,
            })
            .expect("first log page");
        assert_eq!(first.total, 3);
        assert_eq!(first.items.len(), 2);
        let second = store
            .query_logs(&LogQuery {
                cursor: first.next_cursor,
                page_size: 2,
            })
            .expect("second log page");
        assert_eq!(second.items.len(), 1);
        assert!(second.next_cursor.is_none());
    }

    #[test]
    fn structured_logs_redact_document_text_and_absolute_paths() {
        let directory = tempfile::tempdir().expect("tempdir");
        let database_path = directory.path().join("remin.db");
        let store = CatalogStore::open(&database_path).expect("open store");
        let fields = serde_json::json!({
            "document_text": "敏感正文",
            "source_path": r"D:\\资料\\合同.docx",
            "detail": r"C:\\Users\\someone\\secret.txt",
            "status": "ok"
        });
        store
            .append_log(&LogEventInput {
                level: "info",
                component: "test",
                event_name: "privacy.checked",
                job_id: None,
                root_id: None,
                file_id: None,
                fields: &fields,
            })
            .expect("append safe log");

        let connection = Connection::open(database_path).expect("open database");
        let fields_json: String = connection
            .query_row("SELECT fields_json FROM log_events LIMIT 1", [], |row| {
                row.get(0)
            })
            .expect("read fields");
        let stored: serde_json::Value = serde_json::from_str(&fields_json).expect("valid json");
        assert_eq!(stored["document_text"], "[redacted]");
        assert_eq!(stored["source_path"], "[redacted]");
        assert_eq!(stored["detail"], "[redacted_path]");
        assert_eq!(stored["status"], "ok");
    }

    #[test]
    fn scan_job_pause_resume_and_cancel_follow_state_machine() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = CatalogStore::open(directory.path().join("remin.db")).expect("open store");
        let root = store
            .upsert_root(&test_root_registration())
            .expect("insert root");
        let (job, _) = store
            .prepare_scan_job(&root.root_id, "control_test")
            .expect("prepare scan");
        store
            .mark_scan_running(&root.root_id, &job.job_id)
            .expect("mark running");

        assert_eq!(
            store.pause_scan(&job.job_id).expect("pause").status,
            JobStatus::Paused
        );
        assert_eq!(
            store.resume_scan(&job.job_id).expect("resume").status,
            JobStatus::Running
        );
        assert_eq!(
            store.cancel_scan(&job.job_id).expect("cancel").status,
            JobStatus::Cancelled
        );
        assert_eq!(
            store
                .cancel_scan(&job.job_id)
                .expect("idempotent cancel")
                .status,
            JobStatus::Cancelled
        );
    }

    #[test]
    fn candidate_roots_are_idempotent_and_keep_user_decisions() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = CatalogStore::open(directory.path().join("remin.db")).expect("open store");
        let first = store
            .upsert_candidate_root(
                CandidateRootType::Onedrive,
                "C:\\Users\\Test\\OneDrive",
                "c:\\users\\test\\onedrive",
            )
            .expect("insert candidate");
        let ignored = store
            .update_candidate_root_status(&first.candidate_id, CandidateRootStatus::Ignored)
            .expect("ignore candidate");
        let rediscovered = store
            .upsert_candidate_root(
                CandidateRootType::Onedrive,
                "C:\\Users\\Test\\OneDrive",
                "c:\\users\\test\\onedrive",
            )
            .expect("rediscover candidate");

        assert_eq!(first.candidate_id, rediscovered.candidate_id);
        assert_eq!(ignored.status, CandidateRootStatus::Ignored);
        assert_eq!(rediscovered.status, CandidateRootStatus::Ignored);
        assert_eq!(
            store.list_candidate_roots().expect("list candidates").len(),
            1
        );
    }

    #[test]
    fn inbox_collections_and_file_relations_form_a_real_offline_flow() {
        let directory = tempfile::tempdir().expect("tempdir");
        let first_path = directory.path().join("归航计划-最终版.txt");
        let second_path = directory.path().join("归航计划-v2.txt");
        fs::write(&first_path, "离线资料内容一致").expect("write first fixture");
        fs::write(&second_path, "离线资料内容一致").expect("write second fixture");
        let store = CatalogStore::open(directory.path().join("remin.db")).expect("open store");
        let root_path = directory.path().to_string_lossy().to_string();
        let root = store
            .upsert_root(&RootRegistration {
                label: "测试资料".into(),
                canonical_path: root_path.clone(),
                path_key: root_path.to_lowercase(),
                source: RootSource::UserFolder,
                volume_id: "vol-test".into(),
                root_file_id: None,
                authorization_source: AuthorizationSource::UserSelected,
                root_kind: RootKind::Folder,
                volume_type: VolumeType::Fixed,
                watch_mode: WatchMode::Realtime,
            })
            .expect("register root");
        let (job, _) = store
            .prepare_scan_job(&root.root_id, "organizing_test")
            .expect("prepare scan");
        store
            .mark_scan_running(&root.root_id, &job.job_id)
            .expect("start scan");
        let now = Utc::now();
        let discovered = |path: &std::path::Path, name: &str| DiscoveredFile {
            volume_id: "vol-test".into(),
            windows_file_id: None,
            canonical_path: path.to_string_lossy().to_string(),
            path_key: path.to_string_lossy().to_lowercase(),
            name: name.into(),
            extension: "txt".into(),
            mime_type: "text/plain".into(),
            size_bytes: fs::metadata(path).expect("metadata").len(),
            created_at: Some(now),
            modified_at: now,
            relative_path: name.into(),
        };
        store
            .commit_scan(
                &root.root_id,
                &job.job_id,
                &ScanOutcome {
                    files: vec![
                        discovered(&first_path, "归航计划-最终版.txt"),
                        discovered(&second_path, "归航计划-v2.txt"),
                    ],
                    ..ScanOutcome::default()
                },
            )
            .expect("commit scan");

        let inbox = store
            .query_inbox(&InboxQuery {
                status: TriageStatus::New,
                event_types: vec![InboxEventType::Discovered],
                root_ids: vec![root.root_id],
                date_from: None,
                date_to: None,
                cursor: None,
                page_size: 20,
            })
            .expect("query inbox");
        assert_eq!(inbox.items.len(), 2);
        let updated = store
            .update_inbox_item(&InboxUpdateRequest {
                inbox_id: inbox.items[0].inbox_id,
                triage_status: TriageStatus::Reviewed,
            })
            .expect("review inbox item");
        assert_eq!(updated.triage_status, TriageStatus::Reviewed);

        let built_ins = store.list_collections().expect("list built-ins");
        assert_eq!(built_ins.len(), 3);
        assert_eq!(
            built_ins
                .iter()
                .find(|collection| collection.name == "最近7天")
                .expect("recent collection")
                .file_count,
            2
        );
        let manual = store
            .create_collection(&CreateCollectionRequest {
                name: "归航项目".into(),
                description: Some("人工确认的项目资料".into()),
                icon: "folder".into(),
                color: "#8c7cf0".into(),
                kind: CollectionKind::Manual,
                rule: None,
            })
            .expect("create manual collection");
        store
            .add_file_to_collection(&manual.collection_id, &inbox.items[0].file_id)
            .expect("add membership");
        assert_eq!(
            store
                .collection_files(&manual.collection_id)
                .expect("collection files")
                .len(),
            1
        );

        let refresh = store
            .refresh_file_relations(100)
            .expect("refresh relations");
        assert_eq!(refresh.hashed_files, 2);
        assert_eq!(refresh.exact_duplicate_pairs, 1);
        assert_eq!(refresh.version_candidate_pairs, 1);
        let relations = store.list_file_relations(20).expect("list relations");
        assert!(
            relations
                .iter()
                .any(|relation| relation.relation_type == RelationType::ExactDuplicate)
        );
        let first_relation_page = store
            .query_file_relations(&RelationQuery {
                cursor: None,
                page_size: 1,
            })
            .expect("first relation page");
        assert_eq!(first_relation_page.total, relations.len() as u64);
        assert_eq!(first_relation_page.items.len(), 1);
        assert_eq!(
            store
                .query_file_relations(&RelationQuery {
                    cursor: first_relation_page.next_cursor,
                    page_size: 1,
                })
                .expect("second relation page")
                .items
                .len(),
            relations.len().saturating_sub(1).min(1)
        );
        assert_eq!(
            fs::read_to_string(first_path).expect("read source after analysis"),
            "离线资料内容一致"
        );
    }

    #[test]
    fn parsed_chinese_content_is_searchable_with_real_locator() {
        let directory = tempfile::tempdir().expect("tempdir");
        let database_path = directory.path().join("remin.db");
        let store = CatalogStore::open(&database_path).expect("open store");
        let root = store
            .upsert_root(&test_root_registration())
            .expect("insert root");
        let file_id = Uuid::now_v7();
        let revision_id = Uuid::now_v7();
        let now = Utc::now();
        let connection = store.connect().expect("connect");
        connection
            .execute(
                "INSERT INTO files (file_id, canonical_path, path_key, name, extension, size_bytes, modified_at, discovered_at, availability, volume_id, display_name, mime_type, current_revision_id, parse_status, first_seen_at, last_seen_at) VALUES (?1, ?2, ?3, ?4, 'md', 128, ?5, ?5, 'present', 'vol-test', ?4, 'text/markdown', ?6, 'pending', ?5, ?5)",
                params![file_id.to_string(), "C:\\Users\\Test\\Documents\\归航计划.md", "c:\\users\\test\\documents\\归航计划.md", "归航计划.md", now.to_rfc3339(), revision_id.to_string()],
            )
            .expect("insert file");
        connection
            .execute(
                "INSERT INTO file_revisions (revision_id, file_id, size_bytes, fs_modified_at, metadata_fingerprint, created_at) VALUES (?1, ?2, 128, ?3, '128:test', ?3)",
                params![revision_id.to_string(), file_id.to_string(), now.to_rfc3339()],
            )
            .expect("insert revision");
        connection
            .execute(
                "INSERT INTO file_root_memberships (file_id, root_id, relative_path, is_primary) VALUES (?1, ?2, '归航计划.md', 1)",
                params![file_id.to_string(), root.root_id.to_string()],
            )
            .expect("insert membership");
        drop(connection);

        store
            .mark_file_parsing(&file_id, &revision_id)
            .expect("mark parsing");
        assert_eq!(
            store.recover_interrupted_parses().expect("recover parse"),
            1
        );
        assert!(
            store
                .list_pending_parse_files(10)
                .expect("pending parse files")
                .iter()
                .any(|file| file.file_id == file_id)
        );
        store
            .mark_file_parsing(&file_id, &revision_id)
            .expect("restart parsing");

        let parse_result = ParseResult {
            revision_id,
            status: ParseOutcome::Parsed,
            parser_name: "test".into(),
            parser_version: "1".into(),
            nodes: vec![DocumentNode {
                node_id: Uuid::now_v7(),
                parent_id: None,
                ordinal: 1,
                node_type: "paragraph".into(),
                text: Some("项目采用混合召回和重排提升召回率，RRF参数为60。".into()),
                table_data: None,
                locator: SourceLocator {
                    kind: crate::SourceKind::Text,
                    line_start: Some(8),
                    line_end: Some(8),
                    ..SourceLocator::default()
                },
                heading_path: vec!["检索评估".into()],
            }],
            warnings: vec![],
            metrics: crate::ParseMetrics {
                page_count: 0,
                node_count: 1,
                character_count: 28,
                ocr_page_count: 0,
                elapsed_ms: 2,
            },
            error: None,
        };
        store
            .commit_parse_result(&file_id, &parse_result)
            .expect("commit parse result");

        let pending_embeddings = store
            .list_pending_embedding_chunks("embedding-test", 20)
            .expect("list embedding work");
        assert_eq!(pending_embeddings.len(), 1);
        store
            .commit_chunk_embeddings(
                "embedding-test",
                2,
                &[ChunkEmbeddingInput {
                    chunk_id: pending_embeddings[0].chunk_id,
                    vector: vec![1.0, 0.0],
                }],
            )
            .expect("commit embedding");

        let request = crate::SearchRequest {
            query: "混合召回".into(),
            scope: ScopeFilter {
                root_ids: vec![root.root_id],
                collection_ids: vec![],
                file_ids: vec![],
                extensions: vec![],
                modified_from: None,
                modified_to: None,
                availability: crate::Availability::Present,
            },
            mode: SearchMode::Hybrid,
            sort: crate::SearchSort::Relevance,
            page_size: 10,
            cursor: None,
        };
        let session = store.search(&request).expect("search indexed text");

        assert_eq!(session.results.len(), 1);
        assert_eq!(session.results[0].file_id, file_id);
        assert!(
            session.results[0]
                .match_reasons
                .contains(&"fulltext".to_owned())
        );
        assert_eq!(
            session.results[0]
                .locator
                .as_ref()
                .and_then(|locator| locator.line_start),
            Some(8)
        );
        assert_eq!(
            store
                .list_files()
                .expect("list files")
                .first()
                .expect("file")
                .parse_status,
            ParseStatus::Parsed
        );
        let semantic_request = crate::SearchRequest {
            query: "项目回忆".into(),
            scope: request.scope.clone(),
            mode: SearchMode::Semantic,
            sort: crate::SearchSort::Relevance,
            page_size: 10,
            cursor: None,
        };
        let semantic = store
            .search_with_semantic(
                &semantic_request,
                Some(SemanticQuery {
                    model_artifact_id: "embedding-test",
                    vector: &[1.0, 0.0],
                }),
            )
            .expect("semantic search");
        assert_eq!(semantic.results.len(), 1);
        assert!(
            semantic.results[0]
                .match_reasons
                .contains(&"semantic".to_owned())
        );
        assert_eq!(semantic.results[0].scores.semantic, Some(1.0));
        let preview = store.file_preview(&file_id, 10).expect("preview file");
        assert_eq!(preview.nodes.len(), 1);
        assert_eq!(preview.nodes[0].locator.line_start, Some(8));
        assert!(!preview.truncated);
        assert_eq!(
            store
                .authorized_file_path(&file_id)
                .expect("authorized path"),
            PathBuf::from("C:\\Users\\Test\\Documents\\归航计划.md")
        );
        let connection = store.connect().expect("connect for authorization revoke");
        connection
            .execute(
                "UPDATE roots SET enabled = 0 WHERE root_id = ?1",
                [root.root_id.to_string()],
            )
            .expect("disable root");
        assert_eq!(
            store
                .authorized_file_path(&file_id)
                .expect_err("disabled roots cannot open files")
                .code,
            "FILE_NOT_FOUND"
        );
    }

    #[test]
    fn search_cursor_pages_results_and_rejects_a_changed_index() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = CatalogStore::open(directory.path().join("remin.db")).expect("open store");
        let root = store
            .upsert_root(&test_root_registration())
            .expect("insert root");
        let now = Utc::now().to_rfc3339();
        let connection = store.connect().expect("connect");
        let insert_file = |index: usize| {
            let file_id = Uuid::now_v7();
            let path = format!("C:\\Users\\Test\\Documents\\分页资料-{index:02}.txt");
            connection
                  .execute(
                      "INSERT INTO files (file_id, canonical_path, path_key, name, extension, size_bytes, modified_at, discovered_at, availability, volume_id, display_name, mime_type, parse_status, first_seen_at, last_seen_at) VALUES (?1, ?2, ?3, ?4, 'txt', 32, ?5, ?5, 'present', 'vol-test', ?4, 'text/plain', 'pending', ?5, ?5)",
                      params![file_id.to_string(), path, path.to_lowercase(), format!("分页资料-{index:02}.txt"), now],
                  )
                  .expect("insert paged file");
            connection
                  .execute(
                      "INSERT INTO file_root_memberships (file_id, root_id, relative_path, is_primary) VALUES (?1, ?2, ?3, 1)",
                      params![file_id.to_string(), root.root_id.to_string(), format!("分页资料-{index:02}.txt")],
                  )
                  .expect("insert paged membership");
        };
        for index in 0..15 {
            insert_file(index);
        }
        let mut request = crate::SearchRequest {
            query: "分页资料".into(),
            scope: ScopeFilter {
                root_ids: vec![root.root_id],
                collection_ids: vec![],
                file_ids: vec![],
                extensions: vec![],
                modified_from: None,
                modified_to: None,
                availability: crate::Availability::Present,
            },
            mode: SearchMode::Filename,
            sort: crate::SearchSort::NameAsc,
            page_size: 10,
            cursor: None,
        };
        let first = store.search(&request).expect("first search page");
        assert_eq!(first.results.len(), 10);
        let cursor = first.next_cursor.expect("next search cursor");
        request.cursor = Some(cursor.clone());
        let second = store.search(&request).expect("second search page");
        assert_eq!(second.results.len(), 5);
        assert!(second.next_cursor.is_none());

        insert_file(15);
        assert_eq!(
            store
                .search(&request)
                .expect_err("changed index rejects old cursor")
                .code,
            "SEARCH_CURSOR_INVALID"
        );
    }

    #[test]
    #[ignore = "release performance gate; run with scripts/validate_semantic_performance.ps1"]
    fn semantic_search_twenty_thousand_file_gate() {
        const FILE_COUNT: usize = 20_000;
        const DIMENSION: usize = 384;
        let directory = tempfile::tempdir().expect("tempdir");
        let store = CatalogStore::open(directory.path().join("remin.db")).expect("open store");
        let root = store
            .upsert_root(&test_root_registration())
            .expect("insert root");
        let mut connection = store.connect().expect("connect benchmark database");
        let transaction = connection.transaction().expect("begin seed transaction");
        let now = Utc::now().to_rfc3339();
        let locator = serde_json::to_string(&SourceLocator {
            kind: crate::SourceKind::Text,
            line_start: Some(1),
            line_end: Some(1),
            ..SourceLocator::default()
        })
        .expect("serialize locator");
        let mut target_vector = vec![0.0_f32; DIMENSION];
        target_vector[0] = 1.0;
        let mut other_vector = vec![0.0_f32; DIMENSION];
        other_vector[1] = 1.0;
        let target_blob = encode_vector(&target_vector);
        let other_blob = encode_vector(&other_vector);
        for index in 0..FILE_COUNT {
            let file_id = Uuid::now_v7();
            let revision_id = Uuid::now_v7();
            let node_id = Uuid::now_v7();
            let chunk_id = Uuid::now_v7();
            let path = format!("C:\\Users\\Test\\Documents\\基准资料-{index:05}.md");
            transaction
                .execute(
                    "INSERT INTO files (file_id, canonical_path, path_key, name, extension, size_bytes, modified_at, discovered_at, availability, volume_id, display_name, mime_type, fs_created_at, current_revision_id, parse_status, first_seen_at, last_seen_at) VALUES (?1, ?2, ?2, ?3, 'md', 128, ?4, ?4, 'present', 'vol-test', ?3, 'text/markdown', ?4, ?5, 'parsed', ?4, ?4)",
                    params![file_id.to_string(), path.to_ascii_lowercase(), format!("基准资料-{index:05}.md"), now, revision_id.to_string()],
                )
                .expect("insert benchmark file");
            transaction
                .execute(
                    "INSERT INTO file_root_memberships (file_id, root_id, relative_path, is_primary) VALUES (?1, ?2, ?3, 1)",
                    params![file_id.to_string(), root.root_id.to_string(), format!("基准资料-{index:05}.md")],
                )
                .expect("insert benchmark membership");
            transaction
                .execute(
                    "INSERT INTO file_revisions (revision_id, file_id, size_bytes, fs_modified_at, metadata_fingerprint, created_at, parse_status, parser_name, parser_version, index_version, completed_at) VALUES (?1, ?2, 128, ?3, ?4, ?3, 'parsed', 'benchmark', '1', 1, ?3)",
                    params![revision_id.to_string(), file_id.to_string(), now, format!("benchmark-{index}")],
                )
                .expect("insert benchmark revision");
            transaction
                .execute(
                    "INSERT INTO document_nodes (node_id, revision_id, ordinal, node_type, locator_json, heading_path_json, text) VALUES (?1, ?2, 1, 'paragraph', ?3, '[]', ?4)",
                    params![node_id.to_string(), revision_id.to_string(), locator, format!("第{index}份基准资料")],
                )
                .expect("insert benchmark node");
            transaction
                .execute(
                    "INSERT INTO chunks (chunk_id, file_id, revision_id, node_id, ordinal, text, normalized_text, token_count, content_hash, language, locator_json, vector_key, embedding_model_id, embedding_status) VALUES (?1, ?2, ?3, ?4, 1, ?5, ?5, 8, ?6, 'zh', ?7, ?1, 'embedding-benchmark', 'indexed')",
                    params![chunk_id.to_string(), file_id.to_string(), revision_id.to_string(), node_id.to_string(), format!("第{index}份基准资料"), format!("hash-{index}"), locator],
                )
                .expect("insert benchmark chunk");
            transaction
                .execute(
                    "INSERT INTO chunk_embeddings (chunk_id, model_artifact_id, file_id, revision_id, dimension, vector_blob, created_at) VALUES (?1, 'embedding-benchmark', ?2, ?3, ?4, ?5, ?6)",
                    params![chunk_id.to_string(), file_id.to_string(), revision_id.to_string(), DIMENSION as u32, if index == FILE_COUNT - 1 { &target_blob } else { &other_blob }, now],
                )
                .expect("insert benchmark embedding");
        }
        transaction.commit().expect("commit benchmark seed");

        let request = crate::SearchRequest {
            query: "不存在的检索词".into(),
            scope: ScopeFilter {
                root_ids: vec![root.root_id],
                collection_ids: vec![],
                file_ids: vec![],
                extensions: vec![],
                modified_from: None,
                modified_to: None,
                availability: crate::Availability::Present,
            },
            mode: SearchMode::Semantic,
            sort: crate::SearchSort::Relevance,
            page_size: 20,
            cursor: None,
        };
        let mut elapsed_ms = Vec::new();
        for _ in 0..7 {
            let started = std::time::Instant::now();
            let result = store
                .search_with_semantic(
                    &request,
                    Some(SemanticQuery {
                        model_artifact_id: "embedding-benchmark",
                        vector: &target_vector,
                    }),
                )
                .expect("run benchmark search");
            assert_eq!(
                result.results.first().and_then(|item| item.scores.semantic),
                Some(1.0)
            );
            elapsed_ms.push(started.elapsed().as_millis());
        }
        elapsed_ms.sort_unstable();
        let p95_ms = *elapsed_ms.last().expect("benchmark samples");
        let limit_ms = std::env::var("REMIN_SEMANTIC_P95_MS")
            .ok()
            .and_then(|value| value.parse::<u128>().ok())
            .unwrap_or(2_000);
        eprintln!(
            "semantic_search_gate files={FILE_COUNT} dimension={DIMENSION} samples_ms={elapsed_ms:?} p95_ms={p95_ms} limit_ms={limit_ms}"
        );
        assert!(
            p95_ms <= limit_ms,
            "20,000文件语义检索p95={p95_ms}ms，超过门限{limit_ms}ms"
        );
    }
}
