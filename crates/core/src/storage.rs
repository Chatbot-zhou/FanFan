use std::collections::{BTreeMap, HashMap, HashSet};
use std::{fs, fs::File, io::Read, path::PathBuf, time::Duration};

use chrono::{DateTime, Utc};
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OptionalExtension, Transaction, params, params_from_iter};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    AnswerResult, AnswerSourceFile, AppError, AppLogRecord, AskMessage, AskRequest,
    AuthorizationSource, BUILT_IN_EXCLUSION_RULES, CandidateRoot, CandidateRootStatus,
    CandidateRootType, CheckpointStatus, CheckpointType, ChunkEmbeddingInput, CollectionKind,
    CollectionModelReview, CollectionRecord, CollectionRule, CollectionSuggestedMember,
    CollectionSuggestion, CollectionSuggestionPage, CollectionSuggestionQuery,
    CollectionSuggestionRefreshResult, CollectionSuggestionUpdateRequest, CreateCollectionRequest,
    DegradationLevel, DegradationState, DiscoveredFile, DocumentNode, ExclusionRule,
    ExclusionRuleClass, ExclusionRuleInput, ExclusionRuleType, ExplorationCandidate,
    ExtractionChunk, ExtractionDocument, ExtractionRunRequest, ExtractionRunResult,
    ExtractionTable, FilePage, FileQuery, FileRecord, FileRelation, FileSystemEvent,
    HealthCheckItem, ImageAsset, ImageUnderstandingResult, InboxEventType, InboxItem, InboxPage,
    InboxQuery, InboxUpdateRequest, IndexActivityStats, IndexRebuildResult, JobRecord, JobStatus,
    KnowledgeSpace, KnowledgeSpaceRequest, LogPage, LogQuery, MaintenanceSnapshot, ParseOutcome,
    ParseResult, ParseStatus, PendingEmbeddingChunk, PendingImageUnderstanding, RankedHit,
    RelationPage, RelationQuery, RelationRefreshResult, RelationType, RootKind, RootRecord,
    RootSource, RootStatus, ScanOutcome, ScopeFilter, SearchMode, SemanticQuery, SourceLocator,
    TaskPlan, TaskStep, TriageStatus, ValidationCheckpoint, VolumeType, WatchMode,
    chunks_from_nodes, fts_query, normalized_version_key,
};

pub const CURRENT_SCHEMA_VERSION: u32 = 14;

type VectorSourceRow = (u64, String, String, String, Vec<f32>);
type SemanticCandidate = (Uuid, (String, f32));

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
    Migration {
        version: 7,
        name: "persistent_ask_sessions",
        sql: r#"
            CREATE TABLE ask_sessions (
                session_id TEXT PRIMARY KEY,
                scope_json TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE TABLE ask_messages (
                message_id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL REFERENCES ask_sessions(session_id) ON DELETE CASCADE,
                role TEXT NOT NULL,
                content TEXT NOT NULL,
                answer_json TEXT,
                created_at TEXT NOT NULL
            );

            CREATE INDEX idx_ask_messages_session_time
                ON ask_messages(session_id, created_at, message_id);
        "#,
    },
    Migration {
        version: 8,
        name: "ai_collection_suggestions",
        sql: r#"
            ALTER TABLE collection_memberships ADD COLUMN confidence REAL;
            ALTER TABLE collection_memberships ADD COLUMN rationale TEXT;
            ALTER TABLE collection_memberships ADD COLUMN state TEXT NOT NULL DEFAULT 'active';
            ALTER TABLE collection_memberships ADD COLUMN evaluated_at TEXT;

            CREATE TABLE document_profiles (
                file_id TEXT PRIMARY KEY REFERENCES files(file_id) ON DELETE CASCADE,
                revision_id TEXT NOT NULL REFERENCES file_revisions(revision_id) ON DELETE CASCADE,
                title TEXT NOT NULL,
                summary TEXT NOT NULL,
                keywords_json TEXT NOT NULL,
                entities_json TEXT NOT NULL,
                embedding_model_id TEXT NOT NULL,
                dimension INTEGER NOT NULL,
                vector_blob BLOB NOT NULL,
                candidate_bucket TEXT NOT NULL,
                algorithm_version TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE TABLE collection_suggestions (
                suggestion_id TEXT PRIMARY KEY,
                idempotency_key TEXT NOT NULL UNIQUE,
                suggested_name TEXT NOT NULL,
                description TEXT NOT NULL,
                confidence REAL NOT NULL,
                status TEXT NOT NULL DEFAULT 'suggested',
                model_version TEXT NOT NULL,
                algorithm_version TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE TABLE collection_suggested_members (
                suggestion_id TEXT NOT NULL REFERENCES collection_suggestions(suggestion_id) ON DELETE CASCADE,
                file_id TEXT NOT NULL REFERENCES files(file_id) ON DELETE CASCADE,
                revision_id TEXT NOT NULL REFERENCES file_revisions(revision_id) ON DELETE CASCADE,
                confidence REAL NOT NULL,
                rationale TEXT NOT NULL,
                state TEXT NOT NULL DEFAULT 'suggested',
                PRIMARY KEY (suggestion_id, file_id)
            );

            CREATE INDEX idx_document_profiles_bucket ON document_profiles(embedding_model_id, candidate_bucket);
            CREATE INDEX idx_collection_suggestions_status ON collection_suggestions(status, updated_at DESC);
            CREATE INDEX idx_collection_suggested_members_file ON collection_suggested_members(file_id);
        "#,
    },
    Migration {
        version: 9,
        name: "usearch_index_generations",
        sql: r#"
            CREATE TABLE index_generations (
                generation_id TEXT PRIMARY KEY,
                model_artifact_id TEXT NOT NULL,
                dimension INTEGER NOT NULL,
                metric TEXT NOT NULL,
                quantization TEXT NOT NULL,
                index_path TEXT NOT NULL UNIQUE,
                status TEXT NOT NULL,
                item_count INTEGER NOT NULL DEFAULT 0,
                coverage REAL NOT NULL DEFAULT 0,
                error_code TEXT,
                created_at TEXT NOT NULL,
                activated_at TEXT
            );

            CREATE TABLE vector_index_keys (
                generation_id TEXT NOT NULL REFERENCES index_generations(generation_id) ON DELETE CASCADE,
                vector_key INTEGER NOT NULL CHECK (vector_key > 0),
                chunk_id TEXT NOT NULL REFERENCES chunks(chunk_id) ON DELETE CASCADE,
                file_id TEXT NOT NULL REFERENCES files(file_id) ON DELETE CASCADE,
                revision_id TEXT NOT NULL REFERENCES file_revisions(revision_id) ON DELETE CASCADE,
                PRIMARY KEY (generation_id, vector_key),
                UNIQUE (generation_id, chunk_id)
            );

            CREATE UNIQUE INDEX idx_index_generations_active_model
                ON index_generations(model_artifact_id)
                WHERE status = 'active';
            CREATE INDEX idx_vector_index_keys_chunk
                ON vector_index_keys(chunk_id, generation_id);
            CREATE INDEX idx_vector_index_keys_file
                ON vector_index_keys(file_id, generation_id);
        "#,
    },
    Migration {
        version: 10,
        name: "multimodal_image_assets",
        sql: r#"
            CREATE TABLE image_assets (
                asset_id TEXT PRIMARY KEY,
                file_id TEXT NOT NULL REFERENCES files(file_id) ON DELETE CASCADE,
                revision_id TEXT NOT NULL REFERENCES file_revisions(revision_id) ON DELETE CASCADE,
                asset_kind TEXT NOT NULL,
                cache_path TEXT NOT NULL UNIQUE,
                mime_type TEXT NOT NULL,
                size_bytes INTEGER NOT NULL,
                sha256 TEXT NOT NULL,
                locator_json TEXT NOT NULL,
                ocr_text TEXT,
                description TEXT,
                vision_model_id TEXT,
                status TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE INDEX idx_image_assets_revision
                ON image_assets(revision_id, status);
            CREATE INDEX idx_image_assets_pending
                ON image_assets(status, updated_at)
                WHERE status IN ('pending_understanding', 'failed');

            UPDATE file_revisions
                SET parse_status = 'pending', completed_at = NULL, error_code = NULL
                WHERE revision_id IN (
                    SELECT f.current_revision_id FROM files f
                    WHERE f.current_revision_id IS NOT NULL
                      AND f.extension IN ('pdf','docx','docm','xlsx','xlsm','pptx','pptm','jpg','jpeg','png','tif','tiff','bmp','webp')
                )
                  AND COALESCE(index_version, 0) < 2;
            UPDATE files
                SET parse_status = 'pending'
                WHERE current_revision_id IS NOT NULL
                  AND extension IN ('pdf','docx','docm','xlsx','xlsm','pptx','pptm','jpg','jpeg','png','tif','tiff','bmp','webp')
                  AND EXISTS (
                      SELECT 1 FROM file_revisions r
                      WHERE r.revision_id = files.current_revision_id
                        AND COALESCE(r.index_version, 0) < 2
                  );
        "#,
    },
    Migration {
        version: 11,
        name: "metadata_only_file_policy",
        sql: r#"
            UPDATE file_revisions
                SET parse_status = 'unsupported'
                WHERE parse_status = 'pending'
                  AND revision_id IN (
                    SELECT current_revision_id FROM files
                    WHERE current_revision_id IS NOT NULL
                      AND extension NOT IN ('pdf','docx','docm','xlsx','xlsm','pptx','pptm','csv','tsv','md','txt','html','htm','jpg','jpeg','png','tif','tiff','bmp','webp','doc','xls','ppt','zip','rs','py','js','jsx','mjs','cjs','ts','tsx','java','kt','kts','go','c','cc','cpp','h','hpp','cs','rb','php','swift','scala','sh','ps1','sql','json','yaml','yml','toml','xml','css','scss','vue','svelte')
                  );
            UPDATE files
                SET parse_status = 'unsupported'
                WHERE parse_status = 'pending'
                  AND extension NOT IN ('pdf','docx','docm','xlsx','xlsm','pptx','pptm','csv','tsv','md','txt','html','htm','jpg','jpeg','png','tif','tiff','bmp','webp','doc','xls','ppt','zip','rs','py','js','jsx','mjs','cjs','ts','tsx','java','kt','kts','go','c','cc','cpp','h','hpp','cs','rb','php','swift','scala','sh','ps1','sql','json','yaml','yml','toml','xml','css','scss','vue','svelte');
        "#,
    },
    Migration {
        version: 12,
        name: "recoverable_image_understanding",
        sql: r#"
            ALTER TABLE document_nodes ADD COLUMN image_asset_id TEXT REFERENCES image_assets(asset_id) ON DELETE CASCADE;
            ALTER TABLE image_assets ADD COLUMN attempt_count INTEGER NOT NULL DEFAULT 0;
            ALTER TABLE image_assets ADD COLUMN error_json TEXT;
            ALTER TABLE image_assets ADD COLUMN idempotency_key TEXT;
            ALTER TABLE image_assets ADD COLUMN started_at TEXT;
            ALTER TABLE image_assets ADD COLUMN completed_at TEXT;

            CREATE UNIQUE INDEX idx_document_nodes_image_asset
                ON document_nodes(image_asset_id)
                WHERE image_asset_id IS NOT NULL;
            CREATE INDEX idx_image_assets_work_queue
                ON image_assets(status, updated_at, asset_id)
                WHERE status IN ('pending_understanding', 'processing', 'failed');

            UPDATE image_assets
                SET status = 'pending_understanding', started_at = NULL,
                    error_json = NULL, updated_at = CURRENT_TIMESTAMP
                WHERE status = 'processing';
        "#,
    },
    Migration {
        version: 13,
        name: "knowledge_spaces",
        sql: r#"
            CREATE TABLE knowledge_spaces (
                space_id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                description TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE TABLE knowledge_space_roots (
                space_id TEXT NOT NULL REFERENCES knowledge_spaces(space_id) ON DELETE CASCADE,
                root_id TEXT NOT NULL REFERENCES roots(root_id) ON DELETE CASCADE,
                PRIMARY KEY (space_id, root_id)
            );

            CREATE TABLE knowledge_space_collections (
                space_id TEXT NOT NULL REFERENCES knowledge_spaces(space_id) ON DELETE CASCADE,
                collection_id TEXT NOT NULL REFERENCES collections(collection_id) ON DELETE CASCADE,
                PRIMARY KEY (space_id, collection_id)
            );

            CREATE INDEX idx_knowledge_space_roots_root
                ON knowledge_space_roots(root_id, space_id);
            CREATE INDEX idx_knowledge_space_collections_collection
                ON knowledge_space_collections(collection_id, space_id);
        "#,
    },
    Migration {
        version: 14,
        name: "application_settings",
        sql: r#"
            CREATE TABLE application_settings (
                setting_key TEXT PRIMARY KEY,
                value_json TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
        "#,
    },
];

#[derive(Debug, Clone)]
pub struct CatalogStore {
    database_path: PathBuf,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct IndexGeneration {
    pub generation_id: Uuid,
    pub model_artifact_id: String,
    pub dimension: u32,
    pub metric: String,
    pub quantization: String,
    pub status: String,
    pub item_count: u64,
    pub coverage: f64,
    pub error_code: Option<String>,
    pub created_at: DateTime<Utc>,
    pub activated_at: Option<DateTime<Utc>>,
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
        store.recover_interrupted_index_generations()?;
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

    fn vector_index_directory(&self) -> Result<PathBuf, AppError> {
        let parent = self.database_path.parent().ok_or_else(|| {
            AppError::new(
                "VECTOR_INDEX_PATH_INVALID",
                "数据库路径没有可用的父目录",
                false,
            )
        })?;
        Ok(parent.join("vector-indexes"))
    }

    fn recover_interrupted_index_generations(&self) -> Result<(), AppError> {
        let connection = self.connect()?;
        connection
            .execute(
                "UPDATE index_generations SET status = 'failed', error_code = 'VECTOR_INDEX_BUILD_INTERRUPTED' WHERE status = 'building'",
                [],
            )
            .map_err(|error| storage_error("VECTOR_INDEX_GENERATION_RECOVERY_FAILED", error, true))?;
        Ok(())
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

    pub fn storage_quota_override(&self) -> Result<Option<u64>, AppError> {
        let connection = self.connect()?;
        let value = connection
            .query_row(
                "SELECT value_json FROM application_settings WHERE setting_key = 'storage_soft_quota_bytes'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| storage_error("STORAGE_POLICY_READ_FAILED", error, true))?;
        value
            .map(|raw| {
                serde_json::from_str::<u64>(&raw).map_err(|error| {
                    AppError::new("STORAGE_POLICY_INVALID", error.to_string(), false)
                })
            })
            .transpose()
    }

    pub fn set_storage_quota_override(&self, quota_bytes: u64) -> Result<u64, AppError> {
        const GIB: u64 = 1024 * 1024 * 1024;
        if !(GIB..=2 * 1024 * GIB).contains(&quota_bytes) {
            return Err(AppError::new(
                "STORAGE_POLICY_INVALID",
                "存储软配额需要在1GB到2TB之间",
                false,
            ));
        }
        let connection = self.connect()?;
        connection
            .execute(
                "INSERT INTO application_settings (setting_key, value_json, updated_at) VALUES ('storage_soft_quota_bytes', ?1, ?2) ON CONFLICT(setting_key) DO UPDATE SET value_json = excluded.value_json, updated_at = excluded.updated_at",
                params![serde_json::to_string(&quota_bytes).expect("u64 serializes"), Utc::now().to_rfc3339()],
            )
            .map_err(|error| storage_error("STORAGE_POLICY_WRITE_FAILED", error, true))?;
        Ok(quota_bytes)
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

    pub fn upsert_exclusion_rule(
        &self,
        input: &ExclusionRuleInput,
    ) -> Result<ExclusionRule, AppError> {
        input.validate()?;
        let connection = self.connect()?;
        let rule_id = input.rule_id.unwrap_or_else(Uuid::now_v7);
        if input.rule_id.is_some() {
            let built_in_key = connection
                .query_row(
                    "SELECT built_in_key FROM exclusion_rules WHERE rule_id = ?1",
                    [rule_id.to_string()],
                    |row| row.get::<_, Option<String>>(0),
                )
                .optional()
                .map_err(|error| storage_error("EXCLUSION_RULE_MUTATION_FAILED", error, true))?
                .ok_or_else(|| {
                    AppError::new("EXCLUSION_RULE_NOT_FOUND", "排除规则不存在", false)
                })?;
            if built_in_key.is_some() {
                connection.execute("UPDATE exclusion_rules SET enabled = ?1 WHERE rule_id = ?2 AND overridable = 1", params![i64::from(input.enabled), rule_id.to_string()])
                    .map_err(|error| storage_error("EXCLUSION_RULE_MUTATION_FAILED", error, true))?;
            } else {
                connection.execute("UPDATE exclusion_rules SET root_id = ?1, rule_type = ?2, value_json = ?3, enabled = ?4 WHERE rule_id = ?5 AND overridable = 1", params![input.root_id.map(|value| value.to_string()), input.rule_type.as_str(), serde_json::to_string(input.value.trim()).expect("string serializes"), i64::from(input.enabled), rule_id.to_string()])
                    .map_err(|error| storage_error("EXCLUSION_RULE_MUTATION_FAILED", error, true))?;
            }
        } else {
            connection.execute("INSERT INTO exclusion_rules (rule_id, built_in_key, root_id, rule_class, rule_type, value_json, enabled, overridable, created_at) VALUES (?1, NULL, ?2, 'default', ?3, ?4, ?5, 1, ?6)", params![rule_id.to_string(), input.root_id.map(|value| value.to_string()), input.rule_type.as_str(), serde_json::to_string(input.value.trim()).expect("string serializes"), i64::from(input.enabled), Utc::now().to_rfc3339()])
                .map_err(|error| storage_error("EXCLUSION_RULE_MUTATION_FAILED", error, true))?;
        }
        self.list_exclusion_rules()?
            .into_iter()
            .find(|rule| rule.rule_id == rule_id)
            .ok_or_else(|| AppError::new("EXCLUSION_RULE_NOT_FOUND", "排除规则不存在", false))
    }

    pub fn delete_exclusion_rule(&self, rule_id: &Uuid) -> Result<(), AppError> {
        let connection = self.connect()?;
        let changed = connection.execute("DELETE FROM exclusion_rules WHERE rule_id = ?1 AND built_in_key IS NULL AND overridable = 1", [rule_id.to_string()])
            .map_err(|error| storage_error("EXCLUSION_RULE_MUTATION_FAILED", error, true))?;
        if changed == 0 {
            return Err(AppError::new(
                "EXCLUSION_RULE_READONLY",
                "内置或硬排除规则不能删除",
                false,
            ));
        }
        Ok(())
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
        request.validate_filters()?;
        let connection = self.connect()?;
        let offset = request.offset()?;
        let page_size = u64::from(request.validated_page_size());
        let mut predicates = vec![AUTHORIZED_FILE_SQL.to_owned()];
        let mut values = Vec::<SqlValue>::new();
        if let Some(query) = request
            .query
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            predicates.push("f.display_name LIKE ? ESCAPE '\\'".into());
            values.push(SqlValue::Text(format!("%{}%", escape_like(query))));
        }
        if !request.extensions.is_empty() {
            let extensions = request
                .extensions
                .iter()
                .map(|value| value.trim().trim_start_matches('.').to_ascii_lowercase())
                .filter(|value| !value.is_empty())
                .collect::<Vec<_>>();
            if !extensions.is_empty() {
                predicates.push(format!(
                    "lower(f.extension) IN ({})",
                    vec!["?"; extensions.len()].join(",")
                ));
                values.extend(extensions.into_iter().map(SqlValue::Text));
            }
        }
        if !request.parse_statuses.is_empty() {
            let statuses = request
                .parse_statuses
                .iter()
                .filter(|value| {
                    matches!(
                        value.as_str(),
                        "pending" | "parsing" | "parsed" | "ocr_pending" | "failed"
                    )
                })
                .cloned()
                .collect::<Vec<_>>();
            if !statuses.is_empty() {
                predicates.push(format!(
                    "f.parse_status IN ({})",
                    vec!["?"; statuses.len()].join(",")
                ));
                values.extend(statuses.into_iter().map(SqlValue::Text));
            }
        }
        if let Some(availability) = request.availability {
            predicates.push("f.availability = ?".into());
            values.push(SqlValue::Text(availability.as_str().into()));
        }
        let predicate = predicates.join(" AND ");
        let count_sql = format!("SELECT COUNT(*) FROM files f WHERE {predicate}");
        let total = connection
            .query_row(&count_sql, params_from_iter(values.iter()), |row| {
                row.get::<_, u64>(0)
            })
            .map_err(|error| storage_error("FILE_QUERY_FAILED", error, true))?;
        let sql = format!(
            "{FILE_SELECT_WITH_ALIAS} WHERE {predicate} ORDER BY f.last_seen_at DESC, f.file_id DESC LIMIT ? OFFSET ?"
        );
        let mut page_values = values;
        page_values.push(SqlValue::Integer(page_size as i64));
        page_values.push(SqlValue::Integer(offset as i64));
        let mut statement = connection
            .prepare(&sql)
            .map_err(|error| storage_error("FILE_QUERY_FAILED", error, true))?;
        let items = statement
            .query_map(params_from_iter(page_values.iter()), file_from_row)
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

    pub fn list_knowledge_spaces(&self) -> Result<Vec<KnowledgeSpace>, AppError> {
        let connection = self.connect()?;
        let mut statement = connection
            .prepare(
                "SELECT space_id, name, description, created_at, updated_at FROM knowledge_spaces ORDER BY updated_at DESC, space_id DESC",
            )
            .map_err(|error| storage_error("KNOWLEDGE_SPACE_QUERY_FAILED", error, true))?;
        let rows = statement
            .query_map([], |row| {
                let space_id = row.get::<_, String>(0)?;
                let created_at = row.get::<_, String>(3)?;
                let updated_at = row.get::<_, String>(4)?;
                Ok((
                    parse_uuid_column(&space_id, 0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    parse_datetime_column(&created_at, 3)?,
                    parse_datetime_column(&updated_at, 4)?,
                ))
            })
            .map_err(|error| storage_error("KNOWLEDGE_SPACE_QUERY_FAILED", error, true))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| storage_error("KNOWLEDGE_SPACE_QUERY_FAILED", error, true))?;
        drop(statement);

        rows.into_iter()
            .map(|(space_id, name, description, created_at, updated_at)| {
                let (root_ids, collection_ids) = knowledge_space_members(&connection, &space_id)?;
                let file_count =
                    knowledge_space_file_count(&connection, &root_ids, &collection_ids)?;
                Ok(KnowledgeSpace {
                    space_id,
                    name,
                    description,
                    root_ids,
                    collection_ids,
                    file_count,
                    created_at,
                    updated_at,
                })
            })
            .collect()
    }

    pub fn create_knowledge_space(
        &self,
        request: &KnowledgeSpaceRequest,
    ) -> Result<KnowledgeSpace, AppError> {
        request.validate()?;
        let space_id = Uuid::now_v7();
        let now = Utc::now();
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction()
            .map_err(|error| storage_error("DATABASE_TRANSACTION_FAILED", error, true))?;
        validate_knowledge_space_references(&transaction, request)?;
        transaction
            .execute(
                "INSERT INTO knowledge_spaces (space_id, name, description, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?4)",
                params![space_id.to_string(), request.name.trim(), request.description.as_ref().map(|value| value.trim()).filter(|value| !value.is_empty()), now.to_rfc3339()],
            )
            .map_err(|error| storage_error("KNOWLEDGE_SPACE_CREATE_FAILED", error, true))?;
        replace_knowledge_space_members(&transaction, &space_id, request)?;
        transaction
            .commit()
            .map_err(|error| storage_error("DATABASE_COMMIT_FAILED", error, true))?;
        self.knowledge_space_by_id(&space_id)
    }

    pub fn update_knowledge_space(
        &self,
        space_id: &Uuid,
        request: &KnowledgeSpaceRequest,
    ) -> Result<KnowledgeSpace, AppError> {
        request.validate()?;
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction()
            .map_err(|error| storage_error("DATABASE_TRANSACTION_FAILED", error, true))?;
        validate_knowledge_space_references(&transaction, request)?;
        let updated = transaction
            .execute(
                "UPDATE knowledge_spaces SET name = ?2, description = ?3, updated_at = ?4 WHERE space_id = ?1",
                params![space_id.to_string(), request.name.trim(), request.description.as_ref().map(|value| value.trim()).filter(|value| !value.is_empty()), Utc::now().to_rfc3339()],
            )
            .map_err(|error| storage_error("KNOWLEDGE_SPACE_UPDATE_FAILED", error, true))?;
        if updated == 0 {
            return Err(AppError::new(
                "KNOWLEDGE_SPACE_NOT_FOUND",
                "知识空间不存在",
                false,
            ));
        }
        replace_knowledge_space_members(&transaction, space_id, request)?;
        transaction
            .commit()
            .map_err(|error| storage_error("DATABASE_COMMIT_FAILED", error, true))?;
        self.knowledge_space_by_id(space_id)
    }

    pub fn delete_knowledge_space(&self, space_id: &Uuid) -> Result<(), AppError> {
        let connection = self.connect()?;
        let deleted = connection
            .execute(
                "DELETE FROM knowledge_spaces WHERE space_id = ?1",
                [space_id.to_string()],
            )
            .map_err(|error| storage_error("KNOWLEDGE_SPACE_DELETE_FAILED", error, true))?;
        if deleted == 0 {
            return Err(AppError::new(
                "KNOWLEDGE_SPACE_NOT_FOUND",
                "知识空间不存在",
                false,
            ));
        }
        Ok(())
    }

    fn knowledge_space_by_id(&self, space_id: &Uuid) -> Result<KnowledgeSpace, AppError> {
        self.list_knowledge_spaces()?
            .into_iter()
            .find(|space| space.space_id == *space_id)
            .ok_or_else(|| AppError::new("KNOWLEDGE_SPACE_NOT_FOUND", "知识空间不存在", false))
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
        if collection.kind == CollectionKind::Rule {
            return Err(AppError::new(
                "COLLECTION_MEMBERSHIP_INVALID",
                "规则集合由规则自动维护，不能手动添加资料",
                false,
            ));
        }
        authorized_file_by_id(&connection, file_id)?;
        connection
            .execute(
                "INSERT INTO collection_memberships (collection_id, file_id, source, created_at, confidence, rationale, state, evaluated_at) VALUES (?1, ?2, 'manual', ?3, 1.0, '用户手动添加', 'active', ?3) ON CONFLICT(collection_id, file_id) DO UPDATE SET source = 'manual', confidence = 1.0, rationale = '用户手动添加', state = 'active', evaluated_at = excluded.evaluated_at",
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
        if collection.kind == CollectionKind::Rule {
            return Err(AppError::new(
                "COLLECTION_MEMBERSHIP_INVALID",
                "规则集合由规则自动维护，不能手动移除资料",
                false,
            ));
        }
        let changed = if collection.kind == CollectionKind::Ai {
            connection.execute(
                "UPDATE collection_memberships SET state = 'excluded', rationale = '用户人工排除', evaluated_at = ?3 WHERE collection_id = ?1 AND file_id = ?2",
                params![collection_id.to_string(), file_id.to_string(), Utc::now().to_rfc3339()],
            )
        } else {
            connection.execute(
                "DELETE FROM collection_memberships WHERE collection_id = ?1 AND file_id = ?2",
                params![collection_id.to_string(), file_id.to_string()],
            )
        };
        changed.map_err(|error| storage_error("COLLECTION_MEMBERSHIP_FAILED", error, true))?;
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
            CollectionKind::Manual | CollectionKind::Ai => Ok(files
                .into_iter()
                .filter(|file| {
                    connection
                        .query_row(
                            "SELECT EXISTS(SELECT 1 FROM collection_memberships WHERE collection_id = ?1 AND file_id = ?2 AND state = 'active')",
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
            CollectionKind::Manual | CollectionKind::Ai => (
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

    pub fn refresh_collection_suggestions(
        &self,
        model_artifact_id: &str,
        max_files: u32,
    ) -> Result<CollectionSuggestionRefreshResult, AppError> {
        if model_artifact_id.trim().is_empty() || !(1..=2_000).contains(&max_files) {
            return Err(AppError::new(
                "COLLECTION_SUGGESTION_REFRESH_INVALID",
                "AI集合刷新需要有效的Embedding模型，且单批文件数必须在1到2000之间",
                false,
            ));
        }
        const ALGORITHM_VERSION: &str = "semantic_lsh_v1";
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction()
            .map_err(|error| storage_error("COLLECTION_SUGGESTION_REFRESH_FAILED", error, true))?;
        let sql = format!(
            "WITH targets AS (SELECT f.file_id, f.current_revision_id, f.display_name FROM files f LEFT JOIN document_profiles p ON p.file_id = f.file_id WHERE f.current_revision_id IS NOT NULL AND f.parse_status = 'parsed' AND f.availability = 'present' AND {AUTHORIZED_FILE_SQL} AND EXISTS (SELECT 1 FROM chunk_embeddings ce WHERE ce.file_id = f.file_id AND ce.revision_id = f.current_revision_id AND ce.model_artifact_id = ?1) AND (p.file_id IS NULL OR p.revision_id <> f.current_revision_id OR p.embedding_model_id <> ?1 OR p.algorithm_version <> ?2) ORDER BY f.last_seen_at DESC LIMIT ?3) SELECT t.file_id, t.current_revision_id, t.display_name, c.text, e.dimension, e.vector_blob FROM targets t JOIN chunks c ON c.file_id = t.file_id AND c.revision_id = t.current_revision_id JOIN chunk_embeddings e ON e.chunk_id = c.chunk_id AND e.model_artifact_id = ?1 ORDER BY t.file_id, c.ordinal"
        );
        let rows = {
            let mut statement = transaction
                .prepare(&sql)
                .map_err(|error| storage_error("COLLECTION_PROFILE_QUERY_FAILED", error, true))?;
            statement
                .query_map(
                    params![model_artifact_id, ALGORITHM_VERSION, i64::from(max_files)],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, u32>(4)?,
                            row.get::<_, Vec<u8>>(5)?,
                        ))
                    },
                )
                .map_err(|error| storage_error("COLLECTION_PROFILE_QUERY_FAILED", error, true))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| storage_error("COLLECTION_PROFILE_QUERY_FAILED", error, true))?
        };
        let mut grouped: BTreeMap<String, ProfileAggregate> = BTreeMap::new();
        for (file_id, revision_id, title, text, dimension, bytes) in rows {
            let vector = decode_vector(&bytes, dimension)?;
            let entry = grouped
                .entry(file_id)
                .or_insert_with(|| (revision_id, title, Vec::new(), Vec::new()));
            if entry.2.len() < 3 {
                entry.2.push(text);
                entry.3.push(vector);
            }
        }
        let now = Utc::now().to_rfc3339();
        let mut profiles = Vec::new();
        for (file_id, (revision_id, title, texts, vectors)) in grouped {
            let vector = mean_normalized_vector(&vectors)?;
            let bucket = semantic_bucket(&vector);
            let summary = texts
                .first()
                .map(|text| compact_profile_text(text, 260))
                .unwrap_or_default();
            let keywords = profile_keywords(&title);
            transaction
                .execute(
                    "INSERT INTO document_profiles (file_id, revision_id, title, summary, keywords_json, entities_json, embedding_model_id, dimension, vector_blob, candidate_bucket, algorithm_version, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, '[]', ?6, ?7, ?8, ?9, ?10, ?11, ?11) ON CONFLICT(file_id) DO UPDATE SET revision_id = excluded.revision_id, title = excluded.title, summary = excluded.summary, keywords_json = excluded.keywords_json, entities_json = excluded.entities_json, embedding_model_id = excluded.embedding_model_id, dimension = excluded.dimension, vector_blob = excluded.vector_blob, candidate_bucket = excluded.candidate_bucket, algorithm_version = excluded.algorithm_version, updated_at = excluded.updated_at",
                    params![file_id, revision_id, title, summary, serde_json::to_string(&keywords).unwrap_or_else(|_| "[]".into()), model_artifact_id, vector.len() as u32, encode_vector(&vector), bucket, ALGORITHM_VERSION, now],
                )
                .map_err(|error| storage_error("COLLECTION_PROFILE_WRITE_FAILED", error, true))?;
            profiles.push(ProfileCandidate {
                file_id: parse_uuid_value(&file_id)?,
                revision_id: parse_uuid_value(&revision_id)?,
                title,
                vector,
                bucket,
            });
        }
        let mut candidate_edges = 0_u64;
        for profile in &profiles {
            let candidates = {
                let mut statement = transaction
                    .prepare("SELECT file_id, revision_id, title, dimension, vector_blob FROM document_profiles WHERE embedding_model_id = ?1 AND candidate_bucket = ?2 AND file_id <> ?3 ORDER BY updated_at DESC LIMIT 96")
                    .map_err(|error| storage_error("COLLECTION_CANDIDATE_QUERY_FAILED", error, true))?;
                statement
                    .query_map(
                        params![
                            model_artifact_id,
                            profile.bucket,
                            profile.file_id.to_string()
                        ],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, String>(2)?,
                                row.get::<_, u32>(3)?,
                                row.get::<_, Vec<u8>>(4)?,
                            ))
                        },
                    )
                    .map_err(|error| {
                        storage_error("COLLECTION_CANDIDATE_QUERY_FAILED", error, true)
                    })?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|error| {
                        storage_error("COLLECTION_CANDIDATE_QUERY_FAILED", error, true)
                    })?
            };
            for (other_file_id, _other_revision, other_title, dimension, bytes) in candidates {
                let other_file_id = parse_uuid_value(&other_file_id)?;
                let other_vector = decode_vector(&bytes, dimension)?;
                if other_vector.len() != profile.vector.len() {
                    continue;
                }
                let similarity = profile
                    .vector
                    .iter()
                    .zip(&other_vector)
                    .map(|(left, right)| left * right)
                    .sum::<f32>();
                if similarity < 0.78 {
                    continue;
                }
                let (left, right) = if profile.file_id < other_file_id {
                    (profile.file_id, other_file_id)
                } else {
                    (other_file_id, profile.file_id)
                };
                let reasons = serde_json::to_string(&vec![
                    format!("文档语义画像相似度 {:.0}%", similarity * 100.0),
                    format!(
                        "候选桶 {}，对比《{}》与《{}》",
                        profile.bucket, profile.title, other_title
                    ),
                ])
                .map_err(|error| {
                    AppError::new("COLLECTION_REASON_INVALID", error.to_string(), false)
                })?;
                transaction
                    .execute(
                        "INSERT INTO file_relations (relation_id, left_file_id, right_file_id, relation_type, confidence, reasons_json, review_status, created_at, updated_at) VALUES (?1, ?2, ?3, 'related', ?4, ?5, 'suggested', ?6, ?6) ON CONFLICT(left_file_id, right_file_id, relation_type) DO UPDATE SET confidence = excluded.confidence, reasons_json = excluded.reasons_json, updated_at = excluded.updated_at",
                        params![Uuid::now_v7().to_string(), left.to_string(), right.to_string(), f64::from(similarity), reasons, now],
                    )
                    .map_err(|error| storage_error("COLLECTION_CANDIDATE_WRITE_FAILED", error, true))?;
                candidate_edges += 1;
            }
        }
        let mut created_suggestions = 0_u64;
        let mut suggestion_ids = Vec::new();
        let mut consumed = HashSet::new();
        for profile in &profiles {
            if suggestion_ids.len() >= 24 {
                break;
            }
            if consumed.contains(&profile.file_id) {
                continue;
            }
            let related = {
                let mut statement = transaction
                    .prepare("SELECT CASE WHEN left_file_id = ?1 THEN right_file_id ELSE left_file_id END, confidence FROM file_relations WHERE relation_type = 'related' AND review_status <> 'rejected' AND confidence >= 0.78 AND (left_file_id = ?1 OR right_file_id = ?1) ORDER BY confidence DESC LIMIT 11")
                    .map_err(|error| storage_error("COLLECTION_GROUP_QUERY_FAILED", error, true))?;
                statement
                    .query_map([profile.file_id.to_string()], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
                    })
                    .map_err(|error| storage_error("COLLECTION_GROUP_QUERY_FAILED", error, true))?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|error| storage_error("COLLECTION_GROUP_QUERY_FAILED", error, true))?
            };
            if related.is_empty() {
                continue;
            }
            let mut members = vec![(
                profile.file_id,
                profile.revision_id,
                1.0_f64,
                "该文档是本组语义质心候选".to_owned(),
            )];
            for (file_id, confidence) in related {
                let file_id = parse_uuid_value(&file_id)?;
                let revision_id = transaction
                    .query_row(
                        "SELECT revision_id FROM document_profiles WHERE file_id = ?1",
                        [file_id.to_string()],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()
                    .map_err(|error| storage_error("COLLECTION_GROUP_QUERY_FAILED", error, true))?;
                if let Some(revision_id) = revision_id {
                    members.push((
                        file_id,
                        parse_uuid_value(&revision_id)?,
                        confidence,
                        format!("与组内核心文档的语义相似度为 {:.0}%", confidence * 100.0),
                    ));
                }
            }
            members.sort_by_key(|member| member.0);
            members.dedup_by_key(|member| member.0);
            if members.len() < 2 {
                continue;
            }
            let mut digest = Sha256::new();
            digest.update(ALGORITHM_VERSION.as_bytes());
            digest.update(model_artifact_id.as_bytes());
            for (file_id, revision_id, _, _) in &members {
                digest.update(file_id.as_bytes());
                digest.update(revision_id.as_bytes());
            }
            let idempotency_key = format!("{:x}", digest.finalize());
            let suggestion_id = Uuid::now_v7();
            let confidence = members.iter().skip(1).map(|member| member.2).sum::<f64>()
                / (members.len() - 1) as f64;
            let name = format!("{} · 相关资料", collection_name_stem(&profile.title));
            let changed = transaction
                .execute(
                    "INSERT OR IGNORE INTO collection_suggestions (suggestion_id, idempotency_key, suggested_name, description, confidence, status, model_version, algorithm_version, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, 'suggested', ?6, ?7, ?8, ?8)",
                    params![suggestion_id.to_string(), idempotency_key, name, "基于文档级Embedding与内容关系形成的虚拟分组；不会改变任何原文件位置。", confidence, model_artifact_id, ALGORITHM_VERSION, now],
                )
                .map_err(|error| storage_error("COLLECTION_SUGGESTION_WRITE_FAILED", error, true))?;
            if changed == 0 {
                continue;
            }
            for (file_id, revision_id, member_confidence, rationale) in &members {
                transaction
                    .execute(
                        "INSERT INTO collection_suggested_members (suggestion_id, file_id, revision_id, confidence, rationale, state) VALUES (?1, ?2, ?3, ?4, ?5, 'suggested')",
                        params![suggestion_id.to_string(), file_id.to_string(), revision_id.to_string(), member_confidence, rationale],
                    )
                    .map_err(|error| storage_error("COLLECTION_SUGGESTION_WRITE_FAILED", error, true))?;
                insert_inbox_event(
                    &transaction,
                    file_id,
                    InboxEventType::CollectionSuggested,
                    Utc::now(),
                    None,
                    TriageStatus::New,
                    Some("AI发现这份资料与其他文档存在主题联系，等待审核虚拟集合建议"),
                    None,
                    &format!("collection_suggestion:{suggestion_id}:{file_id}"),
                )?;
                consumed.insert(*file_id);
            }
            created_suggestions += 1;
            suggestion_ids.push(suggestion_id);
        }
        transaction
            .commit()
            .map_err(|error| storage_error("COLLECTION_SUGGESTION_REFRESH_FAILED", error, true))?;
        Ok(CollectionSuggestionRefreshResult {
            profiled_files: profiles.len() as u64,
            candidate_edges,
            created_suggestions,
            suggestion_ids,
            algorithm_version: ALGORITHM_VERSION.into(),
            model_version: model_artifact_id.into(),
        })
    }

    pub fn query_collection_suggestions(
        &self,
        request: &CollectionSuggestionQuery,
    ) -> Result<CollectionSuggestionPage, AppError> {
        request.validate()?;
        let connection = self.connect()?;
        let offset = request.offset()?;
        let status = request.status.as_deref().unwrap_or("suggested");
        let total = connection
            .query_row(
                "SELECT COUNT(*) FROM collection_suggestions WHERE status = ?1",
                [status],
                |row| row.get::<_, u64>(0),
            )
            .map_err(|error| storage_error("COLLECTION_SUGGESTION_QUERY_FAILED", error, true))?;
        let raw = {
            let mut statement = connection
                .prepare("SELECT suggestion_id, suggested_name, description, confidence, status, model_version, algorithm_version, created_at, updated_at FROM collection_suggestions WHERE status = ?1 ORDER BY updated_at DESC LIMIT ?2 OFFSET ?3")
                .map_err(|error| storage_error("COLLECTION_SUGGESTION_QUERY_FAILED", error, true))?;
            statement
                .query_map(
                    params![status, i64::from(request.page_size), offset as i64],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, f64>(3)?,
                            row.get::<_, String>(4)?,
                            row.get::<_, String>(5)?,
                            row.get::<_, String>(6)?,
                            row.get::<_, String>(7)?,
                            row.get::<_, String>(8)?,
                        ))
                    },
                )
                .map_err(|error| storage_error("COLLECTION_SUGGESTION_QUERY_FAILED", error, true))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| storage_error("COLLECTION_SUGGESTION_QUERY_FAILED", error, true))?
        };
        let mut items = Vec::new();
        for (
            suggestion_id,
            suggested_name,
            description,
            confidence,
            status,
            model_version,
            algorithm_version,
            created_at,
            updated_at,
        ) in raw
        {
            let suggestion_id = parse_uuid_value(&suggestion_id)?;
            let members = query_suggestion_members(&connection, &suggestion_id)?;
            items.push(CollectionSuggestion {
                suggestion_id,
                suggested_name,
                description,
                confidence,
                status,
                model_version,
                algorithm_version,
                members,
                created_at: parse_datetime_value(&created_at)?,
                updated_at: parse_datetime_value(&updated_at)?,
            });
        }
        let consumed = offset + items.len() as u64;
        Ok(CollectionSuggestionPage {
            items,
            next_cursor: (consumed < total).then(|| consumed.to_string()),
            total,
        })
    }

    pub fn update_collection_suggestion(
        &self,
        suggestion_id: &Uuid,
        request: &CollectionSuggestionUpdateRequest,
    ) -> Result<CollectionSuggestion, AppError> {
        self.update_collection_suggestion_internal(suggestion_id, request, None)
    }

    pub fn apply_collection_model_review(
        &self,
        suggestion_id: &Uuid,
        review: &CollectionModelReview,
        model_version: &str,
    ) -> Result<CollectionSuggestion, AppError> {
        if model_version.trim().is_empty()
            || review.suggested_name.trim().is_empty()
            || review.suggested_name.chars().count() > 40
            || review.description.chars().count() > 400
            || !(2..=50).contains(&review.members.len())
            || review.members.iter().any(|member| {
                member.rationale.trim().is_empty() || member.rationale.chars().count() > 400
            })
            || review
                .members
                .iter()
                .map(|member| member.file_id)
                .collect::<HashSet<_>>()
                .len()
                != review.members.len()
        {
            return Err(AppError::new(
                "COLLECTION_MODEL_REVIEW_INVALID",
                "AI集合复核的名称、说明、成员或理由无效",
                false,
            ));
        }
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction()
            .map_err(|error| storage_error("COLLECTION_MODEL_REVIEW_FAILED", error, true))?;
        ensure_suggestion_status(&transaction, suggestion_id, "suggested")?;
        let existing = query_suggestion_members(&transaction, suggestion_id)?
            .into_iter()
            .map(|member| (member.file.file_id, member))
            .collect::<HashMap<_, _>>();
        if review
            .members
            .iter()
            .any(|member| !existing.contains_key(&member.file_id))
        {
            return Err(AppError::new(
                "COLLECTION_MODEL_REVIEW_INVALID",
                "AI集合复核加入了候选组之外的资料",
                false,
            ));
        }
        let now = Utc::now().to_rfc3339();
        transaction.execute("UPDATE collection_suggestions SET suggested_name = ?1, description = ?2, model_version = ?3, updated_at = ?4 WHERE suggestion_id = ?5", params![review.suggested_name.trim(), review.description.trim(), model_version, now, suggestion_id.to_string()])
            .map_err(|error| storage_error("COLLECTION_MODEL_REVIEW_FAILED", error, true))?;
        transaction
            .execute(
                "DELETE FROM collection_suggested_members WHERE suggestion_id = ?1",
                [suggestion_id.to_string()],
            )
            .map_err(|error| storage_error("COLLECTION_MODEL_REVIEW_FAILED", error, true))?;
        for reviewed_member in &review.members {
            let existing_member = existing
                .get(&reviewed_member.file_id)
                .expect("reviewed member was checked");
            transaction.execute("INSERT INTO collection_suggested_members (suggestion_id, file_id, revision_id, confidence, rationale, state) VALUES (?1, ?2, ?3, ?4, ?5, 'suggested')", params![suggestion_id.to_string(), reviewed_member.file_id.to_string(), existing_member.revision_id.to_string(), existing_member.confidence, reviewed_member.rationale.trim()])
                .map_err(|error| storage_error("COLLECTION_MODEL_REVIEW_FAILED", error, true))?;
        }
        transaction
            .commit()
            .map_err(|error| storage_error("COLLECTION_MODEL_REVIEW_FAILED", error, true))?;
        self.collection_suggestion_by_id(suggestion_id)
    }

    fn update_collection_suggestion_internal(
        &self,
        suggestion_id: &Uuid,
        request: &CollectionSuggestionUpdateRequest,
        model_version: Option<&str>,
    ) -> Result<CollectionSuggestion, AppError> {
        if request.suggested_name.trim().is_empty()
            || request.suggested_name.chars().count() > 40
            || request.description.chars().count() > 400
            || request.member_file_ids.len() < 2
            || request.member_file_ids.len() > 50
        {
            return Err(AppError::new(
                "COLLECTION_SUGGESTION_UPDATE_INVALID",
                "建议名称需为1到40字，且成员数量需为2到50个",
                false,
            ));
        }
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction()
            .map_err(|error| storage_error("COLLECTION_SUGGESTION_UPDATE_FAILED", error, true))?;
        ensure_suggestion_status(&transaction, suggestion_id, "suggested")?;
        transaction.execute("UPDATE collection_suggestions SET suggested_name = ?1, description = ?2, model_version = COALESCE(?3, model_version), updated_at = ?4 WHERE suggestion_id = ?5", params![request.suggested_name.trim(), request.description.trim(), model_version, Utc::now().to_rfc3339(), suggestion_id.to_string()])
            .map_err(|error| storage_error("COLLECTION_SUGGESTION_UPDATE_FAILED", error, true))?;
        let existing = query_suggestion_members(&transaction, suggestion_id)?
            .into_iter()
            .map(|member| (member.file.file_id, member))
            .collect::<HashMap<_, _>>();
        transaction
            .execute(
                "DELETE FROM collection_suggested_members WHERE suggestion_id = ?1",
                [suggestion_id.to_string()],
            )
            .map_err(|error| storage_error("COLLECTION_SUGGESTION_UPDATE_FAILED", error, true))?;
        for file_id in &request.member_file_ids {
            let file = authorized_file_by_id(&transaction, file_id)?;
            let revision_id = file.current_revision_id.ok_or_else(|| {
                AppError::new(
                    "COLLECTION_SUGGESTION_MEMBER_INVALID",
                    "建议成员没有当前修订",
                    true,
                )
            })?;
            let (confidence, rationale, state) = existing
                .get(file_id)
                .map(|member| {
                    (
                        member.confidence,
                        member.rationale.clone(),
                        member.state.clone(),
                    )
                })
                .unwrap_or((1.0, "用户补充到AI建议组".into(), "manual_override".into()));
            transaction.execute("INSERT INTO collection_suggested_members (suggestion_id, file_id, revision_id, confidence, rationale, state) VALUES (?1, ?2, ?3, ?4, ?5, ?6)", params![suggestion_id.to_string(), file_id.to_string(), revision_id.to_string(), confidence, rationale, state])
                .map_err(|error| storage_error("COLLECTION_SUGGESTION_UPDATE_FAILED", error, true))?;
        }
        transaction
            .commit()
            .map_err(|error| storage_error("COLLECTION_SUGGESTION_UPDATE_FAILED", error, true))?;
        self.collection_suggestion_by_id(suggestion_id)
    }

    pub fn confirm_collection_suggestion(
        &self,
        suggestion_id: &Uuid,
    ) -> Result<CollectionRecord, AppError> {
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction()
            .map_err(|error| storage_error("COLLECTION_SUGGESTION_CONFIRM_FAILED", error, true))?;
        ensure_suggestion_status(&transaction, suggestion_id, "suggested")?;
        let (name, description) = transaction.query_row("SELECT suggested_name, description FROM collection_suggestions WHERE suggestion_id = ?1", [suggestion_id.to_string()], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
            .map_err(|error| storage_error("COLLECTION_SUGGESTION_CONFIRM_FAILED", error, true))?;
        let collection_id = Uuid::now_v7();
        let now = Utc::now().to_rfc3339();
        transaction.execute("INSERT INTO collections (collection_id, name, description, icon, color, kind, rule_json, built_in, created_at, updated_at) VALUES (?1, ?2, ?3, 'sparkles', '#8c7cf0', 'ai', NULL, 0, ?4, ?4)", params![collection_id.to_string(), name, description, now])
            .map_err(|error| storage_error("COLLECTION_SUGGESTION_CONFIRM_FAILED", error, false))?;
        let members = query_suggestion_members(&transaction, suggestion_id)?;
        for member in members {
            transaction.execute("INSERT INTO collection_memberships (collection_id, file_id, source, created_at, confidence, rationale, state, evaluated_at) VALUES (?1, ?2, 'model', ?3, ?4, ?5, 'active', ?3)", params![collection_id.to_string(), member.file.file_id.to_string(), now, member.confidence, member.rationale])
                .map_err(|error| storage_error("COLLECTION_SUGGESTION_CONFIRM_FAILED", error, true))?;
        }
        transaction.execute("UPDATE collection_suggestions SET status = 'confirmed', updated_at = ?1 WHERE suggestion_id = ?2", params![now, suggestion_id.to_string()])
            .map_err(|error| storage_error("COLLECTION_SUGGESTION_CONFIRM_FAILED", error, true))?;
        transaction
            .execute(
                "UPDATE inbox_events SET triage_status = 'reviewed', processed_at = ?1 WHERE dedupe_key LIKE ?2 AND triage_status IN ('new','error')",
                params![now, format!("collection_suggestion:{suggestion_id}:%")],
            )
            .map_err(|error| storage_error("COLLECTION_SUGGESTION_CONFIRM_FAILED", error, true))?;
        transaction
            .commit()
            .map_err(|error| storage_error("COLLECTION_SUGGESTION_CONFIRM_FAILED", error, true))?;
        let connection = self.connect()?;
        collection_by_id(&connection, &collection_id)
    }

    pub fn reject_collection_suggestion(&self, suggestion_id: &Uuid) -> Result<(), AppError> {
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction()
            .map_err(|error| storage_error("COLLECTION_SUGGESTION_REJECT_FAILED", error, true))?;
        ensure_suggestion_status(&transaction, suggestion_id, "suggested")?;
        let now = Utc::now().to_rfc3339();
        transaction.execute("UPDATE collection_suggestions SET status = 'rejected', updated_at = ?1 WHERE suggestion_id = ?2", params![now, suggestion_id.to_string()])
            .map_err(|error| storage_error("COLLECTION_SUGGESTION_REJECT_FAILED", error, true))?;
        transaction
            .execute(
                "UPDATE inbox_events SET triage_status = 'reviewed', processed_at = ?1 WHERE dedupe_key LIKE ?2 AND triage_status IN ('new','error')",
                params![now, format!("collection_suggestion:{suggestion_id}:%")],
            )
            .map_err(|error| storage_error("COLLECTION_SUGGESTION_REJECT_FAILED", error, true))?;
        transaction
            .commit()
            .map_err(|error| storage_error("COLLECTION_SUGGESTION_REJECT_FAILED", error, true))
    }

    fn collection_suggestion_by_id(
        &self,
        suggestion_id: &Uuid,
    ) -> Result<CollectionSuggestion, AppError> {
        let connection = self.connect()?;
        let raw = connection.query_row("SELECT suggested_name, description, confidence, status, model_version, algorithm_version, created_at, updated_at FROM collection_suggestions WHERE suggestion_id = ?1", [suggestion_id.to_string()], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, f64>(2)?, row.get::<_, String>(3)?, row.get::<_, String>(4)?, row.get::<_, String>(5)?, row.get::<_, String>(6)?, row.get::<_, String>(7)?)))
            .optional().map_err(|error| storage_error("COLLECTION_SUGGESTION_QUERY_FAILED", error, true))?
            .ok_or_else(|| AppError::new("COLLECTION_SUGGESTION_NOT_FOUND", "AI集合建议不存在", false))?;
        Ok(CollectionSuggestion {
            suggestion_id: *suggestion_id,
            suggested_name: raw.0,
            description: raw.1,
            confidence: raw.2,
            status: raw.3,
            model_version: raw.4,
            algorithm_version: raw.5,
            members: query_suggestion_members(&connection, suggestion_id)?,
            created_at: parse_datetime_value(&raw.6)?,
            updated_at: parse_datetime_value(&raw.7)?,
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
        let mut relation_file_ids = HashSet::new();
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
                    relation_file_ids.insert(group[left_index]);
                    relation_file_ids.insert(group[right_index]);
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
                    relation_file_ids.insert(group[left_index]);
                    relation_file_ids.insert(group[right_index]);
                    version_candidate_pairs += 1;
                }
            }
        }
        for file_id in relation_file_ids {
            let revision_key = files
                .iter()
                .find(|file| file.file_id == file_id)
                .and_then(|file| file.current_revision_id)
                .map(|revision| revision.to_string())
                .unwrap_or_else(|| "no-revision".into());
            insert_inbox_event(
                &transaction,
                &file_id,
                InboxEventType::RelationSuggested,
                Utc::now(),
                None,
                TriageStatus::New,
                Some("关系分析发现重复项或可能的文档版本，等待人工复核"),
                None,
                &format!("relation_suggestion:{file_id}:{revision_key}"),
            )?;
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
                "SELECT file_id, volume_id, canonical_path, display_name, extension, mime_type, size_bytes, fs_created_at, modified_at, windows_file_id, content_sha256, availability, current_revision_id, parse_status, first_seen_at, last_seen_at FROM files WHERE availability = 'present' AND current_revision_id IS NOT NULL AND parse_status = 'pending' AND extension IN ('pdf', 'docx', 'docm', 'xlsx', 'xlsm', 'pptx', 'pptm', 'csv', 'tsv', 'md', 'txt', 'html', 'htm', 'jpg', 'jpeg', 'png', 'tif', 'tiff', 'bmp', 'webp', 'doc', 'xls', 'ppt', 'zip', 'rs', 'py', 'js', 'jsx', 'mjs', 'cjs', 'ts', 'tsx', 'java', 'kt', 'kts', 'go', 'c', 'cc', 'cpp', 'h', 'hpp', 'cs', 'rb', 'php', 'swift', 'scala', 'sh', 'ps1', 'sql', 'json', 'yaml', 'yml', 'toml', 'xml', 'css', 'scss', 'vue', 'svelte') ORDER BY last_seen_at LIMIT ?1",
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

    pub fn active_vector_generation(
        &self,
        model_artifact_id: &str,
    ) -> Result<Option<IndexGeneration>, AppError> {
        let connection = self.connect()?;
        let row = connection
            .query_row(
                "SELECT generation_id, model_artifact_id, dimension, metric, quantization, status, item_count, coverage, error_code, created_at, activated_at FROM index_generations WHERE model_artifact_id = ?1 AND status = 'active' LIMIT 1",
                [model_artifact_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, u32>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, u64>(6)?,
                        row.get::<_, f64>(7)?,
                        row.get::<_, Option<String>>(8)?,
                        row.get::<_, String>(9)?,
                        row.get::<_, Option<String>>(10)?,
                    ))
                },
            )
            .optional()
            .map_err(|error| storage_error("VECTOR_INDEX_GENERATION_QUERY_FAILED", error, true))?;
        let Some((
            generation_id,
            model_artifact_id,
            dimension,
            metric,
            quantization,
            status,
            item_count,
            coverage,
            error_code,
            created_at,
            activated_at,
        )) = row
        else {
            return Ok(None);
        };
        Ok(Some(IndexGeneration {
            generation_id: parse_uuid_value(&generation_id)?,
            model_artifact_id,
            dimension,
            metric,
            quantization,
            status,
            item_count,
            coverage,
            error_code,
            created_at: parse_datetime_value(&created_at)?,
            activated_at: activated_at
                .as_deref()
                .map(parse_datetime_value)
                .transpose()?,
        }))
    }

    pub fn rebuild_vector_generation(
        &self,
        model_artifact_id: &str,
        dimension: u32,
    ) -> Result<IndexGeneration, AppError> {
        if model_artifact_id.trim().is_empty() || dimension == 0 {
            return Err(AppError::new(
                "VECTOR_INDEX_GENERATION_INVALID",
                "向量索引代际需要有效的模型标识和维度",
                false,
            ));
        }
        let generation_id = Uuid::now_v7();
        let index_path = self
            .vector_index_directory()?
            .join(format!("{generation_id}.usearch"));
        let created_at = Utc::now();
        let mut connection = self.connect()?;
        connection
            .execute(
                "INSERT INTO index_generations (generation_id, model_artifact_id, dimension, metric, quantization, index_path, status, item_count, coverage, created_at) VALUES (?1, ?2, ?3, 'cosine', 'bf16', ?4, 'building', 0, 0, ?5)",
                params![generation_id.to_string(), model_artifact_id, dimension, index_path.to_string_lossy(), created_at.to_rfc3339()],
            )
            .map_err(|error| storage_error("VECTOR_INDEX_GENERATION_WRITE_FAILED", error, true))?;

        let loaded = (|| -> Result<Vec<VectorSourceRow>, AppError> {
            let sql = format!(
                "SELECT e.chunk_id, e.file_id, e.revision_id, e.vector_blob FROM chunk_embeddings e JOIN files f ON f.file_id = e.file_id WHERE e.model_artifact_id = ?1 AND e.dimension = ?2 AND f.current_revision_id = e.revision_id AND f.availability = 'present' AND {AUTHORIZED_FILE_SQL} ORDER BY e.chunk_id"
            );
            let mut statement = connection
                .prepare(&sql)
                .map_err(|error| storage_error("VECTOR_INDEX_SOURCE_QUERY_FAILED", error, true))?;
            let rows = statement
                .query_map(params![model_artifact_id, dimension], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Vec<u8>>(3)?,
                    ))
                })
                .map_err(|error| storage_error("VECTOR_INDEX_SOURCE_QUERY_FAILED", error, true))?;
            let mut loaded = Vec::new();
            for (index, row) in rows.enumerate() {
                let (chunk_id, file_id, revision_id, bytes) = row.map_err(|error| {
                    storage_error("VECTOR_INDEX_SOURCE_QUERY_FAILED", error, true)
                })?;
                let key = u64::try_from(index + 1).map_err(|_| {
                    AppError::new("VECTOR_INDEX_CAPACITY_EXCEEDED", "向量键空间不足", false)
                })?;
                loaded.push((
                    key,
                    chunk_id,
                    file_id,
                    revision_id,
                    decode_vector(&bytes, dimension)?,
                ));
            }
            Ok(loaded)
        })();
        let loaded = match loaded {
            Ok(loaded) if !loaded.is_empty() => loaded,
            Ok(_) => {
                let error = AppError::new(
                    "VECTOR_INDEX_EMPTY",
                    "当前模型还没有可建立索引的有效向量",
                    true,
                );
                let _ = connection.execute(
                    "UPDATE index_generations SET status = 'failed', error_code = ?2 WHERE generation_id = ?1",
                    params![generation_id.to_string(), error.code.clone()],
                );
                return Err(error);
            }
            Err(error) => {
                let _ = connection.execute(
                    "UPDATE index_generations SET status = 'failed', error_code = ?2 WHERE generation_id = ?1",
                    params![generation_id.to_string(), error.code.clone()],
                );
                return Err(error);
            }
        };
        let entries = loaded
            .iter()
            .map(|(key, _, _, _, vector)| (*key, vector.as_slice()))
            .collect::<Vec<_>>();
        if let Err(error) = crate::build_index_refs(&index_path, dimension as usize, &entries) {
            let _ = connection.execute(
                "UPDATE index_generations SET status = 'failed', error_code = ?2 WHERE generation_id = ?1",
                params![generation_id.to_string(), error.code.clone()],
            );
            return Err(error);
        }

        let total_chunks = count_query(
            &connection,
            &format!(
                "SELECT COUNT(*) FROM chunks c JOIN files f ON f.file_id = c.file_id WHERE f.current_revision_id = c.revision_id AND f.availability = 'present' AND {AUTHORIZED_FILE_SQL}"
            ),
        )?;
        let coverage = coverage_ratio(loaded.len() as u64, total_chunks);
        let activated_at = Utc::now();
        let transaction = connection
            .transaction()
            .map_err(|error| storage_error("DATABASE_TRANSACTION_FAILED", error, true))?;
        for (key, chunk_id, file_id, revision_id, _) in &loaded {
            let key = i64::try_from(*key).map_err(|_| {
                AppError::new(
                    "VECTOR_INDEX_CAPACITY_EXCEEDED",
                    "向量键超出数据库范围",
                    false,
                )
            })?;
            transaction
                .execute(
                    "INSERT INTO vector_index_keys (generation_id, vector_key, chunk_id, file_id, revision_id) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![generation_id.to_string(), key, chunk_id, file_id, revision_id],
                )
                .map_err(|error| storage_error("VECTOR_INDEX_KEY_WRITE_FAILED", error, true))?;
        }
        transaction
            .execute(
                "UPDATE index_generations SET status = 'retired' WHERE model_artifact_id = ?1 AND status = 'active'",
                [model_artifact_id],
            )
            .map_err(|error| storage_error("VECTOR_INDEX_GENERATION_WRITE_FAILED", error, true))?;
        transaction
            .execute(
                "UPDATE index_generations SET status = 'active', item_count = ?2, coverage = ?3, activated_at = ?4, error_code = NULL WHERE generation_id = ?1 AND status = 'building'",
                params![generation_id.to_string(), loaded.len() as u64, coverage, activated_at.to_rfc3339()],
            )
            .map_err(|error| storage_error("VECTOR_INDEX_GENERATION_WRITE_FAILED", error, true))?;
        transaction
            .commit()
            .map_err(|error| storage_error("DATABASE_COMMIT_FAILED", error, true))?;
        Ok(IndexGeneration {
            generation_id,
            model_artifact_id: model_artifact_id.to_owned(),
            dimension,
            metric: "cosine".into(),
            quantization: "bf16".into(),
            status: "active".into(),
            item_count: loaded.len() as u64,
            coverage,
            error_code: None,
            created_at,
            activated_at: Some(activated_at),
        })
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
        for asset in &result.image_assets {
            let cache_path = PathBuf::from(&asset.cache_path);
            let path_is_managed = cache_path.components().any(|component| {
                component
                    .as_os_str()
                    .to_string_lossy()
                    .eq_ignore_ascii_case("image-assets")
            });
            let metadata = fs::metadata(&cache_path).map_err(|error| {
                AppError::new("IMAGE_ASSET_CACHE_UNAVAILABLE", error.to_string(), true)
            })?;
            if asset.revision_id != result.revision_id
                || !path_is_managed
                || !metadata.is_file()
                || metadata.len() != asset.size_bytes
                || !matches!(
                    asset.status.as_str(),
                    "pending_understanding" | "ready" | "failed"
                )
                || hash_file_sha256(&cache_path)? != asset.sha256
            {
                return Err(AppError::new(
                    "IMAGE_ASSET_INVALID",
                    "图片缓存的修订、路径、大小、哈希或状态无效",
                    false,
                ));
            }
        }
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
        transaction
            .execute(
                "DELETE FROM image_assets WHERE revision_id = ?1",
                [result.revision_id.to_string()],
            )
            .map_err(|error| storage_error("IMAGE_ASSET_WRITE_FAILED", error, true))?;

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
        for asset in &result.image_assets {
            let locator_json = serde_json::to_string(&asset.locator)
                .map_err(|error| AppError::new("IMAGE_ASSET_INVALID", error.to_string(), false))?;
            let now = Utc::now().to_rfc3339();
            transaction
                .execute(
                    "INSERT INTO image_assets (asset_id, file_id, revision_id, asset_kind, cache_path, mime_type, size_bytes, sha256, locator_json, ocr_text, description, vision_model_id, status, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?14)",
                    params![asset.asset_id.to_string(), file_id.to_string(), asset.revision_id.to_string(), asset.asset_kind, asset.cache_path, asset.mime_type, asset.size_bytes, asset.sha256, locator_json, asset.ocr_text, asset.description, asset.vision_model_id, asset.status, now],
                )
                .map_err(|error| storage_error("IMAGE_ASSET_WRITE_FAILED", error, true))?;
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

    pub fn recover_interrupted_image_understanding(&self) -> Result<u64, AppError> {
        let connection = self.connect()?;
        connection
            .execute(
                "UPDATE image_assets SET status = 'pending_understanding', started_at = NULL, updated_at = ?1, error_json = ?2 WHERE status = 'processing'",
                params![Utc::now().to_rfc3339(), serde_json::to_string(&AppError::new("VISION_JOB_INTERRUPTED", "应用上次退出时图片理解尚未完成，已从检查点恢复", true)).expect("serialize static error")],
            )
            .map(|changed| changed as u64)
            .map_err(|error| storage_error("IMAGE_UNDERSTANDING_RECOVERY_FAILED", error, true))
    }

    pub fn claim_pending_image_understanding(
        &self,
        model_artifact_id: &str,
    ) -> Result<Option<PendingImageUnderstanding>, AppError> {
        if model_artifact_id.trim().is_empty() {
            return Err(AppError::new(
                "VISION_MODEL_INVALID",
                "图片理解任务缺少模型标识",
                false,
            ));
        }
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction()
            .map_err(|error| storage_error("IMAGE_UNDERSTANDING_CLAIM_FAILED", error, true))?;
        let sql = format!(
            "SELECT ia.asset_id, ia.file_id, ia.revision_id, ia.cache_path, ia.mime_type, ia.size_bytes, ia.sha256, ia.locator_json, ia.ocr_text, ia.attempt_count FROM image_assets ia JOIN files f ON f.file_id = ia.file_id WHERE ia.status = 'pending_understanding' AND f.current_revision_id = ia.revision_id AND f.availability = 'present' AND {AUTHORIZED_FILE_SQL} ORDER BY ia.updated_at, ia.asset_id LIMIT 1"
        );
        let row = transaction
            .query_row(&sql, [], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, u64>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, Option<String>>(8)?,
                    row.get::<_, u32>(9)?,
                ))
            })
            .optional()
            .map_err(|error| storage_error("IMAGE_UNDERSTANDING_CLAIM_FAILED", error, true))?;
        let Some((
            asset_id,
            file_id,
            revision_id,
            cache_path,
            mime_type,
            size_bytes,
            sha256,
            locator_json,
            ocr_text,
            attempt_count,
        )) = row
        else {
            return Ok(None);
        };
        let asset_id = parse_uuid_value(&asset_id)?;
        let file_id = parse_uuid_value(&file_id)?;
        let revision_id = parse_uuid_value(&revision_id)?;
        let locator = serde_json::from_str::<SourceLocator>(&locator_json)
            .map_err(|error| AppError::new("IMAGE_ASSET_INVALID", error.to_string(), false))?;
        let path = PathBuf::from(&cache_path);
        let metadata = fs::metadata(&path)
            .map_err(|error| AppError::new("IMAGE_ASSET_UNAVAILABLE", error.to_string(), true))?;
        if !metadata.is_file() || metadata.len() != size_bytes || hash_file_sha256(&path)? != sha256
        {
            transaction
                .execute(
                    "UPDATE image_assets SET status = 'failed', error_json = ?1, updated_at = ?2 WHERE asset_id = ?3",
                    params![serde_json::to_string(&AppError::new("IMAGE_ASSET_CACHE_INVALID", "图片缓存大小或哈希已经变化，需要重新解析源文件", false)).expect("serialize static error"), Utc::now().to_rfc3339(), asset_id.to_string()],
                )
                .map_err(|error| storage_error("IMAGE_UNDERSTANDING_CLAIM_FAILED", error, true))?;
            transaction
                .commit()
                .map_err(|error| storage_error("DATABASE_COMMIT_FAILED", error, true))?;
            return Ok(None);
        }
        let idempotency_key = format!("vision:v1:{model_artifact_id}:{sha256}");
        let now = Utc::now().to_rfc3339();
        let changed = transaction
            .execute(
                "UPDATE image_assets SET status = 'processing', attempt_count = attempt_count + 1, error_json = NULL, idempotency_key = ?1, started_at = ?2, completed_at = NULL, updated_at = ?2 WHERE asset_id = ?3 AND status = 'pending_understanding'",
                params![idempotency_key, now, asset_id.to_string()],
            )
            .map_err(|error| storage_error("IMAGE_UNDERSTANDING_CLAIM_FAILED", error, true))?;
        if changed != 1 {
            return Err(AppError::new(
                "IMAGE_UNDERSTANDING_ALREADY_CLAIMED",
                "图片理解任务已由另一个后台执行器领取",
                true,
            ));
        }
        transaction
            .commit()
            .map_err(|error| storage_error("DATABASE_COMMIT_FAILED", error, true))?;
        Ok(Some(PendingImageUnderstanding {
            asset_id,
            file_id,
            revision_id,
            cache_path,
            mime_type,
            size_bytes,
            sha256,
            locator,
            ocr_text,
            attempt_count: attempt_count.saturating_add(1),
            idempotency_key,
        }))
    }

    pub fn fail_image_understanding(
        &self,
        asset_id: &Uuid,
        error: &AppError,
    ) -> Result<(), AppError> {
        let connection = self.connect()?;
        let attempts = connection
            .query_row(
                "SELECT attempt_count FROM image_assets WHERE asset_id = ?1",
                [asset_id.to_string()],
                |row| row.get::<_, u32>(0),
            )
            .optional()
            .map_err(|query_error| {
                storage_error("IMAGE_UNDERSTANDING_FAIL_FAILED", query_error, true)
            })?
            .ok_or_else(|| AppError::new("IMAGE_ASSET_NOT_FOUND", "图片理解任务不存在", false))?;
        let status = if error.retryable && attempts < 2 {
            "pending_understanding"
        } else {
            "failed"
        };
        connection
            .execute(
                "UPDATE image_assets SET status = ?1, error_json = ?2, started_at = NULL, updated_at = ?3 WHERE asset_id = ?4 AND status = 'processing'",
                params![status, serde_json::to_string(error).map_err(|serialize_error| AppError::new("IMAGE_UNDERSTANDING_ERROR_INVALID", serialize_error.to_string(), false))?, Utc::now().to_rfc3339(), asset_id.to_string()],
            )
            .map_err(|write_error| storage_error("IMAGE_UNDERSTANDING_FAIL_FAILED", write_error, true))?;
        Ok(())
    }

    pub fn retry_image_understanding(&self, asset_id: &Uuid) -> Result<(), AppError> {
        let connection = self.connect()?;
        let sql = format!(
            "UPDATE image_assets SET status = 'pending_understanding', attempt_count = 0, error_json = NULL, started_at = NULL, completed_at = NULL, updated_at = ?1 WHERE asset_id = ?2 AND status = 'failed' AND EXISTS (SELECT 1 FROM files f WHERE f.file_id = image_assets.file_id AND f.current_revision_id = image_assets.revision_id AND f.availability = 'present' AND {AUTHORIZED_FILE_SQL})"
        );
        let changed = connection
            .execute(&sql, params![Utc::now().to_rfc3339(), asset_id.to_string()])
            .map_err(|error| storage_error("IMAGE_UNDERSTANDING_RETRY_FAILED", error, true))?;
        if changed == 0 {
            return Err(AppError::new(
                "IMAGE_UNDERSTANDING_RETRY_INVALID",
                "只有失败的图片理解任务可以重试",
                false,
            ));
        }
        Ok(())
    }

    pub fn commit_image_understanding(
        &self,
        result: &ImageUnderstandingResult,
    ) -> Result<(), AppError> {
        let summary = result.summary.trim();
        if summary.is_empty()
            || summary.chars().count() > 12_000
            || result.keywords.len() > 64
            || result.entities.len() > 64
            || result.idempotency_key.trim().is_empty()
        {
            return Err(AppError::new(
                "IMAGE_UNDERSTANDING_RESULT_INVALID",
                "图片理解结果为空或超出安全长度",
                false,
            ));
        }
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction()
            .map_err(|error| storage_error("IMAGE_UNDERSTANDING_COMMIT_FAILED", error, true))?;
        let sql = format!(
            "SELECT ia.file_id, ia.revision_id, ia.locator_json, ia.ocr_text, ia.status, ia.idempotency_key FROM image_assets ia JOIN files f ON f.file_id = ia.file_id WHERE ia.asset_id = ?1 AND f.current_revision_id = ia.revision_id AND f.availability = 'present' AND {AUTHORIZED_FILE_SQL} LIMIT 1"
        );
        let row = transaction
            .query_row(&sql, [result.asset_id.to_string()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<String>>(5)?,
                ))
            })
            .optional()
            .map_err(|error| storage_error("IMAGE_UNDERSTANDING_COMMIT_FAILED", error, true))?
            .ok_or_else(|| {
                AppError::new(
                    "IMAGE_UNDERSTANDING_STALE_REVISION",
                    "图片所属文件已经变化、离线或超出授权范围，结果未写入",
                    false,
                )
            })?;
        let (file_id, revision_id, locator_json, existing_ocr, status, stored_key) = row;
        let revision_id = parse_uuid_value(&revision_id)?;
        if revision_id != result.revision_id {
            return Err(AppError::new(
                "IMAGE_UNDERSTANDING_STALE_REVISION",
                "图片理解结果不属于当前文件修订",
                false,
            ));
        }
        if status == "ready" && stored_key.as_deref() == Some(result.idempotency_key.as_str()) {
            return Ok(());
        }
        if status != "processing" || stored_key.as_deref() != Some(result.idempotency_key.as_str())
        {
            return Err(AppError::new(
                "IMAGE_UNDERSTANDING_CHECKPOINT_MISMATCH",
                "图片理解任务检查点已经变化，结果未写入",
                true,
            ));
        }
        let file_id = parse_uuid_value(&file_id)?;
        let locator = serde_json::from_str::<SourceLocator>(&locator_json)
            .map_err(|error| AppError::new("IMAGE_ASSET_INVALID", error.to_string(), false))?;
        let visible_text = result
            .visible_text
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .or(existing_ocr
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty()));
        let mut sections = vec![format!("图片内容：{summary}")];
        if let Some(value) = visible_text {
            sections.push(format!("图片文字：{value}"));
        }
        if let Some(value) = result
            .chart_summary
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            sections.push(format!("图表信息：{value}"));
        }
        if !result.keywords.is_empty() {
            sections.push(format!("关键词：{}", result.keywords.join("、")));
        }
        if !result.entities.is_empty() {
            sections.push(format!("实体：{}", result.entities.join("、")));
        }
        let node_id = result.asset_id;
        transaction
            .execute(
                "DELETE FROM chunks_fts WHERE chunk_id IN (SELECT chunk_id FROM chunks WHERE node_id IN (SELECT node_id FROM document_nodes WHERE image_asset_id = ?1))",
                [result.asset_id.to_string()],
            )
            .map_err(|error| storage_error("IMAGE_UNDERSTANDING_COMMIT_FAILED", error, true))?;
        transaction
            .execute(
                "DELETE FROM document_nodes WHERE image_asset_id = ?1",
                [result.asset_id.to_string()],
            )
            .map_err(|error| storage_error("IMAGE_UNDERSTANDING_COMMIT_FAILED", error, true))?;
        let node_ordinal = transaction
            .query_row(
                "SELECT COALESCE(MAX(ordinal), 0) + 1 FROM document_nodes WHERE revision_id = ?1",
                [revision_id.to_string()],
                |row| row.get::<_, u64>(0),
            )
            .map_err(|error| storage_error("IMAGE_UNDERSTANDING_COMMIT_FAILED", error, true))?;
        let node = DocumentNode {
            node_id,
            parent_id: None,
            ordinal: node_ordinal,
            node_type: "image_description".into(),
            text: Some(sections.join("\n")),
            table_data: None,
            locator: locator.clone(),
            heading_path: vec!["图片理解".into()],
        };
        let parse_result = ParseResult {
            revision_id,
            status: ParseOutcome::Parsed,
            parser_name: "vision".into(),
            parser_version: "1".into(),
            nodes: vec![node.clone()],
            image_assets: vec![],
            warnings: vec![],
            metrics: crate::ParseMetrics {
                page_count: 0,
                node_count: 1,
                character_count: node
                    .text
                    .as_ref()
                    .map_or(0, |text| text.chars().count() as u64),
                ocr_page_count: 0,
                elapsed_ms: 0,
            },
            error: None,
        };
        let mut chunks = chunks_from_nodes(&parse_result);
        let chunk_ordinal = transaction
            .query_row(
                "SELECT COALESCE(MAX(ordinal), 0) FROM chunks WHERE revision_id = ?1",
                [revision_id.to_string()],
                |row| row.get::<_, u64>(0),
            )
            .map_err(|error| storage_error("IMAGE_UNDERSTANDING_COMMIT_FAILED", error, true))?;
        transaction
            .execute(
                "INSERT INTO document_nodes (node_id, revision_id, parent_id, ordinal, node_type, locator_json, heading_path_json, text, table_json, image_asset_id) VALUES (?1, ?2, NULL, ?3, 'image_description', ?4, ?5, ?6, NULL, ?1)",
                params![node_id.to_string(), revision_id.to_string(), node_ordinal, locator_json, serde_json::to_string(&node.heading_path).expect("serialize static heading"), node.text],
            )
            .map_err(|error| storage_error("IMAGE_UNDERSTANDING_COMMIT_FAILED", error, true))?;
        for chunk in &mut chunks {
            chunk.ordinal += chunk_ordinal;
            let chunk_locator = serde_json::to_string(&chunk.locator).map_err(|error| {
                AppError::new("INDEX_SERIALIZE_FAILED", error.to_string(), false)
            })?;
            transaction
                .execute(
                    "INSERT INTO chunks (chunk_id, file_id, revision_id, node_id, ordinal, text, normalized_text, token_count, content_hash, language, locator_json, embedding_model_id, embedding_status) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, NULL, 'pending')",
                    params![chunk.chunk_id.to_string(), file_id.to_string(), revision_id.to_string(), node_id.to_string(), chunk.ordinal, chunk.text, chunk.normalized_text, chunk.token_count, chunk.content_hash, chunk.language, chunk_locator],
                )
                .map_err(|error| storage_error("IMAGE_UNDERSTANDING_COMMIT_FAILED", error, true))?;
            transaction
                .execute(
                    "INSERT INTO chunks_fts (chunk_id, file_id, revision_id, normalized_text) VALUES (?1, ?2, ?3, ?4)",
                    params![chunk.chunk_id.to_string(), file_id.to_string(), revision_id.to_string(), chunk.normalized_text],
                )
                .map_err(|error| storage_error("IMAGE_UNDERSTANDING_COMMIT_FAILED", error, true))?;
        }
        let completed_at = Utc::now().to_rfc3339();
        let changed = transaction
            .execute(
                "UPDATE image_assets SET description = ?1, ocr_text = COALESCE(?2, ocr_text), vision_model_id = ?3, status = 'ready', error_json = NULL, completed_at = ?4, updated_at = ?4 WHERE asset_id = ?5 AND revision_id = ?6 AND status = 'processing' AND idempotency_key = ?7",
                params![summary, result.visible_text, result.model_artifact_id, completed_at, result.asset_id.to_string(), revision_id.to_string(), result.idempotency_key],
            )
            .map_err(|error| storage_error("IMAGE_UNDERSTANDING_COMMIT_FAILED", error, true))?;
        if changed != 1 {
            return Err(AppError::new(
                "IMAGE_UNDERSTANDING_CHECKPOINT_MISMATCH",
                "图片理解任务在提交时已经变化",
                true,
            ));
        }
        transaction
            .commit()
            .map_err(|error| storage_error("DATABASE_COMMIT_FAILED", error, true))
    }

    pub fn image_understanding_stats(&self) -> Result<(u64, u64, u64), AppError> {
        let connection = self.connect()?;
        let sql = format!(
            "SELECT COUNT(*), SUM(CASE WHEN ia.status = 'ready' THEN 1 ELSE 0 END), SUM(CASE WHEN ia.status IN ('pending_understanding','processing') THEN 1 ELSE 0 END) FROM image_assets ia JOIN files f ON f.file_id = ia.file_id WHERE f.current_revision_id = ia.revision_id AND f.availability = 'present' AND {AUTHORIZED_FILE_SQL}"
        );
        connection
            .query_row(&sql, [], |row| {
                Ok((
                    row.get::<_, u64>(0)?,
                    row.get::<_, Option<u64>>(1)?.unwrap_or(0),
                    row.get::<_, Option<u64>>(2)?.unwrap_or(0),
                ))
            })
            .map_err(|error| storage_error("IMAGE_UNDERSTANDING_STATS_FAILED", error, true))
    }

    pub fn search(&self, request: &crate::SearchRequest) -> Result<crate::SearchSession, AppError> {
        self.search_with_semantic(request, None)
    }

    pub fn semantic_index_coverage(
        &self,
        scope: &ScopeFilter,
        model_artifact_id: &str,
    ) -> Result<(f64, f64), AppError> {
        if model_artifact_id.trim().is_empty() {
            return Ok((0.0, 0.0));
        }
        let connection = self.connect()?;
        let files = list_files_with_connection(&connection)?;
        let scoped_file_ids = collect_scoped_file_ids(&connection, &files, scope)?;
        let sql = format!(
            "SELECT c.file_id, EXISTS (SELECT 1 FROM chunk_embeddings e WHERE e.chunk_id = c.chunk_id AND e.model_artifact_id = ?1 AND e.revision_id = c.revision_id) FROM chunks c JOIN files f ON f.file_id = c.file_id WHERE f.current_revision_id = c.revision_id AND f.availability = 'present' AND {AUTHORIZED_FILE_SQL}"
        );
        let mut statement = connection
            .prepare(&sql)
            .map_err(|error| storage_error("RAG_COVERAGE_QUERY_FAILED", error, true))?;
        let rows = statement
            .query_map([model_artifact_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?))
            })
            .map_err(|error| storage_error("RAG_COVERAGE_QUERY_FAILED", error, true))?;
        let mut total = 0_u64;
        let mut indexed = 0_u64;
        let mut scoped_total = 0_u64;
        let mut scoped_indexed = 0_u64;
        for row in rows {
            let (file_id, has_embedding) =
                row.map_err(|error| storage_error("RAG_COVERAGE_QUERY_FAILED", error, true))?;
            total += 1;
            indexed += u64::from(has_embedding);
            if Uuid::parse_str(&file_id).is_ok_and(|file_id| scoped_file_ids.contains(&file_id)) {
                scoped_total += 1;
                scoped_indexed += u64::from(has_embedding);
            }
        }
        Ok((
            coverage_ratio(indexed, total),
            coverage_ratio(scoped_indexed, scoped_total),
        ))
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
                    "SELECT c.chunk_id, c.node_id, c.text, c.locator_json, n.image_asset_id FROM chunks c JOIN document_nodes n ON n.node_id = c.node_id WHERE c.file_id = ?1 AND c.revision_id = ?2 AND c.text = ?3 LIMIT 1",
                    params![result.file_id.to_string(), revision_id.to_string(), result.snippet],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, String>(3)?, row.get::<_, Option<String>>(4)?)),
                )
                .optional()
                .map_err(|error| storage_error("ASK_EVIDENCE_QUERY_FAILED", error, true))?;
            let Some((chunk_id, node_id, quote, locator_json, image_asset_id)) = row else {
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
                    image_asset_id: image_asset_id
                        .as_deref()
                        .map(parse_uuid_value)
                        .transpose()?,
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

    pub fn load_ask_history(
        &self,
        session_id: &Uuid,
        limit: usize,
    ) -> Result<Vec<AskMessage>, AppError> {
        let connection = self.connect()?;
        let mut statement = connection
            .prepare(
                "SELECT message_id, role, content, answer_json, created_at FROM ask_messages WHERE session_id = ?1 ORDER BY created_at DESC, message_id DESC LIMIT ?2",
            )
            .map_err(|error| storage_error("ASK_HISTORY_QUERY_FAILED", error, true))?;
        let rows = statement
            .query_map(
                params![session_id.to_string(), limit.clamp(1, 20) as i64],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                },
            )
            .map_err(|error| storage_error("ASK_HISTORY_QUERY_FAILED", error, true))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| storage_error("ASK_HISTORY_QUERY_FAILED", error, true))?;
        let mut messages = rows
            .into_iter()
            .map(|(message_id, role, content, answer_json, created_at)| {
                Ok(AskMessage {
                    message_id: parse_uuid_value(&message_id)?,
                    session_id: *session_id,
                    role,
                    content,
                    answer: answer_json
                        .map(|value| {
                            serde_json::from_str(&value).map_err(|error| {
                                AppError::new("ASK_HISTORY_INVALID", error.to_string(), false)
                            })
                        })
                        .transpose()?,
                    created_at: parse_datetime_value(&created_at)?,
                })
            })
            .collect::<Result<Vec<_>, AppError>>()?;
        messages.reverse();
        Ok(messages)
    }

    pub fn record_ask_exchange(
        &self,
        request: &AskRequest,
        result: &AnswerResult,
    ) -> Result<(), AppError> {
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction()
            .map_err(|error| storage_error("ASK_HISTORY_WRITE_FAILED", error, true))?;
        let now = Utc::now();
        let scope_json = serde_json::to_string(&request.scope)
            .map_err(|error| AppError::new("ASK_SCOPE_INVALID", error.to_string(), false))?;
        transaction
            .execute(
                "INSERT INTO ask_sessions (session_id, scope_json, created_at, updated_at) VALUES (?1, ?2, ?3, ?3) ON CONFLICT(session_id) DO UPDATE SET scope_json = excluded.scope_json, updated_at = excluded.updated_at",
                params![result.session_id.to_string(), scope_json, now.to_rfc3339()],
            )
            .map_err(|error| storage_error("ASK_HISTORY_WRITE_FAILED", error, true))?;
        transaction
            .execute(
                "INSERT INTO ask_messages (message_id, session_id, role, content, answer_json, created_at) VALUES (?1, ?2, 'user', ?3, NULL, ?4)",
                params![Uuid::now_v7().to_string(), result.session_id.to_string(), request.question.trim(), now.to_rfc3339()],
            )
            .map_err(|error| storage_error("ASK_HISTORY_WRITE_FAILED", error, true))?;
        let answer_json = serde_json::to_string(result)
            .map_err(|error| AppError::new("ASK_RESULT_INVALID", error.to_string(), false))?;
        transaction
            .execute(
                "INSERT INTO ask_messages (message_id, session_id, role, content, answer_json, created_at) VALUES (?1, ?2, 'assistant', ?3, ?4, ?5)",
                params![result.message_id.to_string(), result.session_id.to_string(), result.answer, answer_json, Utc::now().to_rfc3339()],
            )
            .map_err(|error| storage_error("ASK_HISTORY_WRITE_FAILED", error, true))?;
        transaction
            .commit()
            .map_err(|error| storage_error("ASK_HISTORY_WRITE_FAILED", error, true))
    }

    pub fn validate_answer_evidence(&self, answer: &AnswerResult) -> Result<(), AppError> {
        let connection = self.connect()?;
        for citation in answer.claims.iter().flat_map(|claim| &claim.citations) {
            let valid: bool = connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM chunks c JOIN document_nodes n ON n.node_id = c.node_id JOIN files f ON f.file_id = c.file_id JOIN file_root_memberships m ON m.file_id = f.file_id JOIN roots r ON r.root_id = m.root_id WHERE c.chunk_id = ?1 AND c.node_id = ?2 AND c.file_id = ?3 AND c.revision_id = ?4 AND c.text = ?5 AND (?6 IS NULL OR n.image_asset_id = ?6) AND f.current_revision_id = ?4 AND f.availability = 'present' AND r.enabled = 1)",
                    params![citation.chunk_id.to_string(), citation.node_id.to_string(), citation.file_id.to_string(), citation.revision_id.to_string(), citation.quote, citation.image_asset_id.map(|value| value.to_string())],
                    |row| row.get(0),
                )
                .map_err(|error| storage_error("ASK_CITATION_VALIDATE_FAILED", error, true))?;
            if !valid {
                return Err(AppError::new(
                    "ASK_CITATION_STALE_OR_UNAUTHORIZED",
                    "回答引用的资料已变化、离线或不再位于授权范围，已停止显示回答",
                    true,
                ));
            }
        }
        Ok(())
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
                image_assets: vec![],
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
        let image_assets = image_assets_for_revision(&connection, &revision_id)?;
        let next_offset = truncated.then_some((effective_offset + nodes.len()) as u32);
        Ok(crate::FilePreview {
            file,
            revision_id: Some(revision_id),
            nodes,
            image_assets,
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

    pub fn authorized_image_asset_path(
        &self,
        asset_id: &Uuid,
    ) -> Result<(PathBuf, String, u64), AppError> {
        let connection = self.connect()?;
        let sql = format!(
            "SELECT ia.cache_path, ia.mime_type, ia.size_bytes, ia.sha256 FROM image_assets ia JOIN files f ON f.file_id = ia.file_id WHERE ia.asset_id = ?1 AND f.current_revision_id = ia.revision_id AND f.availability = 'present' AND {AUTHORIZED_FILE_SQL} LIMIT 1"
        );
        let (cache_path, mime_type, size_bytes, sha256) = connection
            .query_row(&sql, [asset_id.to_string()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, u64>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })
            .optional()
            .map_err(|error| storage_error("IMAGE_ASSET_QUERY_FAILED", error, true))?
            .ok_or_else(|| {
                AppError::new(
                    "IMAGE_ASSET_NOT_FOUND",
                    "图片资产不存在、已经失效或超出授权范围",
                    false,
                )
            })?;
        if !matches!(
            mime_type.as_str(),
            "image/jpeg" | "image/png" | "image/tiff" | "image/bmp" | "image/webp"
        ) || size_bytes == 0
            || size_bytes > 64 * 1024 * 1024
        {
            return Err(AppError::new(
                "IMAGE_ASSET_UNSAFE",
                "图片资产类型或大小不允许在预览中加载",
                false,
            ));
        }
        let path = PathBuf::from(cache_path);
        let metadata = fs::metadata(&path)
            .map_err(|error| AppError::new("IMAGE_ASSET_UNAVAILABLE", error.to_string(), true))?;
        if !metadata.is_file() || metadata.len() != size_bytes || hash_file_sha256(&path)? != sha256
        {
            return Err(AppError::new(
                "IMAGE_ASSET_UNAVAILABLE",
                "图片资产缓存已经失效，需要重新建立索引",
                true,
            ));
        }
        Ok((path, mime_type, size_bytes))
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
        let background_notice = degradation
            .triggers
            .first()
            .cloned()
            .or_else(|| (active_jobs > 0).then(|| format!("{active_jobs}个后台任务正在处理")));
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
            background_notice,
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
                    fields: sanitize_log_value(
                        &serde_json::from_str(&fields_json).map_err(|error| {
                            AppError::new("LOG_DATA_INVALID", error.to_string(), false)
                        })?,
                        None,
                    ),
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
            if matches!(text.as_str(), "full" | "balanced" | "core") =>
        {
            serde_json::Value::String("background_adjusted".to_owned())
        }
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
                .filter(|(key, _)| {
                    let key = key.to_ascii_lowercase();
                    !key.contains("degradation")
                        && !key.contains("resource_mode")
                        && !key.contains("runtime_mode")
                        && key != "triggers"
                        && key != "disabled_features"
                })
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
        || rule.modified_within_days.is_some()
        || rule.min_size_bytes.is_some()
        || rule.max_size_bytes.is_some();
    if rule.excludes_metadata(file) {
        return Ok(false);
    }
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
    if !rule.exclude_text_keywords.is_empty()
        && let Some(revision_id) = file.current_revision_id
    {
        for keyword in &rule.exclude_text_keywords {
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
                return Ok(false);
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
            CollectionKind::Manual | CollectionKind::Ai => connection
                .query_row(
                    "SELECT COUNT(*) FROM collection_memberships m WHERE m.collection_id = ?1 AND m.state = 'active' AND EXISTS (SELECT 1 FROM file_root_memberships fm JOIN roots r ON r.root_id = fm.root_id WHERE fm.file_id = m.file_id AND r.enabled = 1)",
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
                "SELECT COUNT(*) FROM files f JOIN collection_memberships cm ON cm.file_id = f.file_id WHERE cm.collection_id = ?1 AND cm.state = 'active' AND {AUTHORIZED_FILE_SQL}"
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
        "{FILE_SELECT_WITH_ALIAS} JOIN collection_memberships cm ON cm.file_id = f.file_id WHERE cm.collection_id = ?1 AND cm.state = 'active' AND {AUTHORIZED_FILE_SQL} ORDER BY f.last_seen_at DESC LIMIT ?2 OFFSET ?3"
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

#[derive(Debug, Clone)]
struct ProfileCandidate {
    file_id: Uuid,
    revision_id: Uuid,
    title: String,
    vector: Vec<f32>,
    bucket: String,
}

type ProfileAggregate = (String, String, Vec<String>, Vec<Vec<f32>>);

fn query_suggestion_members(
    connection: &Connection,
    suggestion_id: &Uuid,
) -> Result<Vec<CollectionSuggestedMember>, AppError> {
    let sql = format!(
        "SELECT f.file_id, f.volume_id, f.canonical_path, f.display_name, f.extension, f.mime_type, f.size_bytes, f.fs_created_at, f.modified_at, f.windows_file_id, f.content_sha256, f.availability, f.current_revision_id, f.parse_status, f.first_seen_at, f.last_seen_at, sm.revision_id, sm.confidence, sm.rationale, sm.state FROM files f JOIN collection_suggested_members sm ON sm.file_id = f.file_id WHERE sm.suggestion_id = ?1 AND {AUTHORIZED_FILE_SQL} ORDER BY sm.confidence DESC, f.display_name LIMIT 50"
    );
    let mut statement = connection
        .prepare(&sql)
        .map_err(|error| storage_error("COLLECTION_SUGGESTION_QUERY_FAILED", error, true))?;
    statement
        .query_map([suggestion_id.to_string()], |row| {
            let file = file_from_row(row)?;
            Ok((
                file,
                (
                    row.get::<_, String>(16)?,
                    row.get::<_, f64>(17)?,
                    row.get::<_, String>(18)?,
                    row.get::<_, String>(19)?,
                ),
            ))
        })
        .map_err(|error| storage_error("COLLECTION_SUGGESTION_QUERY_FAILED", error, true))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| storage_error("COLLECTION_SUGGESTION_QUERY_FAILED", error, true))?
        .into_iter()
        .map(|(file, (revision_id, confidence, rationale, state))| {
            Ok(CollectionSuggestedMember {
                file,
                revision_id: parse_uuid_value(&revision_id)?,
                confidence,
                rationale,
                state,
            })
        })
        .collect()
}

fn coverage_ratio(indexed: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        (indexed as f64 / total as f64).clamp(0.0, 1.0)
    }
}

fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn ensure_suggestion_status(
    connection: &Connection,
    suggestion_id: &Uuid,
    expected: &str,
) -> Result<(), AppError> {
    let status = connection
        .query_row(
            "SELECT status FROM collection_suggestions WHERE suggestion_id = ?1",
            [suggestion_id.to_string()],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|error| storage_error("COLLECTION_SUGGESTION_QUERY_FAILED", error, true))?
        .ok_or_else(|| {
            AppError::new("COLLECTION_SUGGESTION_NOT_FOUND", "AI集合建议不存在", false)
        })?;
    if status != expected {
        return Err(AppError::new(
            "COLLECTION_SUGGESTION_STATE_INVALID",
            format!("AI集合建议当前状态为{status}，不能执行此操作"),
            false,
        ));
    }
    Ok(())
}

fn decode_vector(bytes: &[u8], dimension: u32) -> Result<Vec<f32>, AppError> {
    if bytes.len() != dimension as usize * 4 {
        return Err(AppError::new(
            "EMBEDDING_VECTOR_INVALID",
            "向量字节长度与维度不一致",
            false,
        ));
    }
    let vector = bytes
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect::<Vec<_>>();
    if vector.iter().any(|value| !value.is_finite()) {
        return Err(AppError::new(
            "EMBEDDING_VECTOR_INVALID",
            "向量包含无效数值",
            false,
        ));
    }
    Ok(vector)
}

fn mean_normalized_vector(vectors: &[Vec<f32>]) -> Result<Vec<f32>, AppError> {
    let dimension = vectors.first().map(Vec::len).unwrap_or_default();
    if dimension == 0 || vectors.iter().any(|vector| vector.len() != dimension) {
        return Err(AppError::new(
            "COLLECTION_PROFILE_VECTOR_INVALID",
            "文档向量维度不一致",
            false,
        ));
    }
    let mut mean = vec![0.0_f32; dimension];
    for vector in vectors {
        for (target, value) in mean.iter_mut().zip(vector) {
            *target += *value;
        }
    }
    let norm = mean.iter().map(|value| value * value).sum::<f32>().sqrt();
    if !norm.is_finite() || norm <= f32::EPSILON {
        return Err(AppError::new(
            "COLLECTION_PROFILE_VECTOR_INVALID",
            "文档向量无法归一化",
            false,
        ));
    }
    mean.iter_mut().for_each(|value| *value /= norm);
    Ok(mean)
}

fn semantic_bucket(vector: &[f32]) -> String {
    let bits = vector
        .iter()
        .take(6)
        .enumerate()
        .fold(0_u8, |value, (index, item)| {
            value | (u8::from(*item >= 0.0) << index)
        });
    format!("{bits:02x}")
}

fn compact_profile_text(value: &str, limit: usize) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(limit)
        .collect()
}

fn collection_name_stem(value: &str) -> String {
    let stem = value
        .rsplit_once('.')
        .map_or(value, |(stem, _)| stem)
        .trim();
    let compact = compact_profile_text(stem, 24);
    if compact.is_empty() {
        "相关主题".into()
    } else {
        compact
    }
}

fn profile_keywords(value: &str) -> Vec<String> {
    let stem = collection_name_stem(value);
    let chars = stem.chars().collect::<Vec<_>>();
    let mut keywords = Vec::new();
    for window in chars.windows(2).take(8) {
        let keyword = window.iter().collect::<String>();
        if !keywords.contains(&keyword) {
            keywords.push(keyword);
        }
    }
    keywords
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
    if rule.min_size_bytes.is_some() || rule.max_size_bytes.is_some() {
        let mut size_conditions = Vec::new();
        if let Some(minimum) = rule.min_size_bytes {
            size_conditions.push("f.size_bytes >= ?");
            values.push(SqlValue::Integer(minimum.min(i64::MAX as u64) as i64));
        }
        if let Some(maximum) = rule.max_size_bytes {
            size_conditions.push("f.size_bytes <= ?");
            values.push(SqlValue::Integer(maximum.min(i64::MAX as u64) as i64));
        }
        conditions.push(format!("({})", size_conditions.join(" AND ")));
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
    let mut predicate = if conditions.is_empty() {
        "1 = 1".to_owned()
    } else {
        format!("({})", conditions.join(separator))
    };
    let mut exclusions = Vec::new();
    if !rule.exclude_extensions.is_empty() {
        exclusions.push(format!(
            "lower(f.extension) IN ({})",
            vec!["?"; rule.exclude_extensions.len()].join(",")
        ));
        values.extend(
            rule.exclude_extensions
                .iter()
                .map(|value| SqlValue::Text(value.trim_start_matches('.').to_lowercase())),
        );
    }
    if !rule.exclude_filename_keywords.is_empty() {
        exclusions.push(format!(
            "({})",
            vec!["lower(f.display_name) LIKE ?"; rule.exclude_filename_keywords.len()].join(" OR ")
        ));
        values.extend(
            rule.exclude_filename_keywords
                .iter()
                .map(|value| SqlValue::Text(format!("%{}%", value.to_lowercase()))),
        );
    }
    if !rule.exclude_path_keywords.is_empty() {
        exclusions.push(format!(
            "({})",
            vec!["lower(f.canonical_path) LIKE ?"; rule.exclude_path_keywords.len()].join(" OR ")
        ));
        values.extend(
            rule.exclude_path_keywords
                .iter()
                .map(|value| SqlValue::Text(format!("%{}%", value.to_lowercase()))),
        );
    }
    if !rule.exclude_text_keywords.is_empty() {
        exclusions.push(format!(
            "EXISTS (SELECT 1 FROM chunks rc WHERE rc.revision_id = f.current_revision_id AND ({}))",
            vec!["lower(rc.text) LIKE ?"; rule.exclude_text_keywords.len()].join(" OR ")
        ));
        values.extend(
            rule.exclude_text_keywords
                .iter()
                .map(|value| SqlValue::Text(format!("%{}%", value.to_lowercase()))),
        );
    }
    if !exclusions.is_empty() {
        predicate.push_str(&format!(" AND NOT ({})", exclusions.join(" OR ")));
    }
    (predicate, values)
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

fn image_assets_for_revision(
    connection: &Connection,
    revision_id: &Uuid,
) -> Result<Vec<ImageAsset>, AppError> {
    let mut statement = connection
        .prepare(
            "SELECT asset_id, revision_id, asset_kind, cache_path, mime_type, size_bytes, sha256, locator_json, ocr_text, description, vision_model_id, status, error_json FROM image_assets WHERE revision_id = ?1 ORDER BY created_at, asset_id LIMIT 512",
        )
        .map_err(|error| storage_error("IMAGE_ASSET_QUERY_FAILED", error, true))?;
    statement
        .query_map([revision_id.to_string()], |row| {
            let asset_id = row.get::<_, String>(0)?;
            let revision_id = row.get::<_, String>(1)?;
            let locator_json = row.get::<_, String>(7)?;
            Ok(ImageAsset {
                asset_id: parse_uuid_column(&asset_id, 0)?,
                revision_id: parse_uuid_column(&revision_id, 1)?,
                asset_kind: row.get(2)?,
                cache_path: row.get(3)?,
                mime_type: row.get(4)?,
                size_bytes: row.get(5)?,
                sha256: row.get(6)?,
                locator: parse_json_column(7, &locator_json)?,
                ocr_text: row.get(8)?,
                description: row.get(9)?,
                vision_model_id: row.get(10)?,
                status: row.get(11)?,
                error: row
                    .get::<_, Option<String>>(12)?
                    .map(|value| parse_json_column(12, &value))
                    .transpose()?,
            })
        })
        .map_err(|error| storage_error("IMAGE_ASSET_QUERY_FAILED", error, true))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| storage_error("IMAGE_ASSET_QUERY_FAILED", error, true))
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

fn validate_knowledge_space_references(
    transaction: &Transaction<'_>,
    request: &KnowledgeSpaceRequest,
) -> Result<(), AppError> {
    for root_id in request.root_ids.iter().collect::<HashSet<_>>() {
        let exists = transaction
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM roots WHERE root_id = ?1 AND enabled = 1)",
                [root_id.to_string()],
                |row| row.get::<_, i64>(0),
            )
            .map_err(|error| storage_error("KNOWLEDGE_SPACE_REFERENCE_FAILED", error, true))?
            != 0;
        if !exists {
            return Err(AppError::new(
                "KNOWLEDGE_SPACE_ROOT_INVALID",
                "知识空间包含不存在或已停用的资料位置",
                false,
            ));
        }
    }
    for collection_id in request.collection_ids.iter().collect::<HashSet<_>>() {
        let exists = transaction
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM collections WHERE collection_id = ?1)",
                [collection_id.to_string()],
                |row| row.get::<_, i64>(0),
            )
            .map_err(|error| storage_error("KNOWLEDGE_SPACE_REFERENCE_FAILED", error, true))?
            != 0;
        if !exists {
            return Err(AppError::new(
                "KNOWLEDGE_SPACE_COLLECTION_INVALID",
                "知识空间包含不存在的集合",
                false,
            ));
        }
    }
    Ok(())
}

fn replace_knowledge_space_members(
    transaction: &Transaction<'_>,
    space_id: &Uuid,
    request: &KnowledgeSpaceRequest,
) -> Result<(), AppError> {
    transaction
        .execute(
            "DELETE FROM knowledge_space_roots WHERE space_id = ?1",
            [space_id.to_string()],
        )
        .map_err(|error| storage_error("KNOWLEDGE_SPACE_UPDATE_FAILED", error, true))?;
    transaction
        .execute(
            "DELETE FROM knowledge_space_collections WHERE space_id = ?1",
            [space_id.to_string()],
        )
        .map_err(|error| storage_error("KNOWLEDGE_SPACE_UPDATE_FAILED", error, true))?;
    for root_id in request.root_ids.iter().collect::<HashSet<_>>() {
        transaction
            .execute(
                "INSERT INTO knowledge_space_roots (space_id, root_id) VALUES (?1, ?2)",
                params![space_id.to_string(), root_id.to_string()],
            )
            .map_err(|error| storage_error("KNOWLEDGE_SPACE_UPDATE_FAILED", error, true))?;
    }
    for collection_id in request.collection_ids.iter().collect::<HashSet<_>>() {
        transaction
            .execute(
                "INSERT INTO knowledge_space_collections (space_id, collection_id) VALUES (?1, ?2)",
                params![space_id.to_string(), collection_id.to_string()],
            )
            .map_err(|error| storage_error("KNOWLEDGE_SPACE_UPDATE_FAILED", error, true))?;
    }
    Ok(())
}

fn knowledge_space_members(
    connection: &Connection,
    space_id: &Uuid,
) -> Result<(Vec<Uuid>, Vec<Uuid>), AppError> {
    let load_ids = |query: &str, code: &'static str| -> Result<Vec<Uuid>, AppError> {
        let mut statement = connection
            .prepare(query)
            .map_err(|error| storage_error(code, error, true))?;
        statement
            .query_map([space_id.to_string()], |row| {
                let value = row.get::<_, String>(0)?;
                parse_uuid_column(&value, 0)
            })
            .map_err(|error| storage_error(code, error, true))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| storage_error(code, error, true))
    };
    Ok((
        load_ids(
            "SELECT root_id FROM knowledge_space_roots WHERE space_id = ?1 ORDER BY root_id",
            "KNOWLEDGE_SPACE_ROOT_QUERY_FAILED",
        )?,
        load_ids(
            "SELECT collection_id FROM knowledge_space_collections WHERE space_id = ?1 ORDER BY collection_id",
            "KNOWLEDGE_SPACE_COLLECTION_QUERY_FAILED",
        )?,
    ))
}

fn file_matches_any_root(
    connection: &Connection,
    file_id: &Uuid,
    root_ids: &[Uuid],
) -> Result<bool, AppError> {
    if root_ids.is_empty() {
        return Ok(false);
    }
    let allowed = root_ids.iter().map(Uuid::to_string).collect::<HashSet<_>>();
    let mut statement = connection
        .prepare(
            "SELECT m.root_id FROM file_root_memberships m JOIN roots r ON r.root_id = m.root_id WHERE m.file_id = ?1 AND r.enabled = 1",
        )
        .map_err(|error| storage_error("KNOWLEDGE_SPACE_SCOPE_FAILED", error, true))?;
    let roots = statement
        .query_map([file_id.to_string()], |row| row.get::<_, String>(0))
        .map_err(|error| storage_error("KNOWLEDGE_SPACE_SCOPE_FAILED", error, true))?;
    for root in roots {
        if allowed.contains(
            &root.map_err(|error| storage_error("KNOWLEDGE_SPACE_SCOPE_FAILED", error, true))?,
        ) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn knowledge_space_contains_file(
    connection: &Connection,
    file: &FileRecord,
    space_id: &Uuid,
) -> Result<bool, AppError> {
    let exists = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM knowledge_spaces WHERE space_id = ?1)",
            [space_id.to_string()],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|error| storage_error("KNOWLEDGE_SPACE_SCOPE_FAILED", error, true))?
        != 0;
    if !exists {
        return Err(AppError::new(
            "KNOWLEDGE_SPACE_NOT_FOUND",
            "检索范围中的知识空间不存在",
            false,
        ));
    }
    let (root_ids, collection_ids) = knowledge_space_members(connection, space_id)?;
    Ok(file_matches_any_root(connection, &file.file_id, &root_ids)?
        || (!collection_ids.is_empty()
            && file_matches_collection_scope(connection, file, &collection_ids)?))
}

fn file_matches_knowledge_space_scope(
    connection: &Connection,
    file: &FileRecord,
    space_ids: &[Uuid],
) -> Result<bool, AppError> {
    for space_id in space_ids {
        if knowledge_space_contains_file(connection, file, space_id)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn knowledge_space_file_count(
    connection: &Connection,
    root_ids: &[Uuid],
    collection_ids: &[Uuid],
) -> Result<u64, AppError> {
    let files = list_files_with_connection(connection)?;
    let mut count = 0_u64;
    for file in files {
        if !file_is_authorized(connection, &file.file_id)?
            || file.availability != crate::Availability::Present
        {
            continue;
        }
        if file_matches_any_root(connection, &file.file_id, root_ids)?
            || (!collection_ids.is_empty()
                && file_matches_collection_scope(connection, &file, collection_ids)?)
        {
            count += 1;
        }
    }
    Ok(count)
}

fn file_matches_scope(
    connection: &Connection,
    file: &FileRecord,
    scope: &ScopeFilter,
) -> Result<bool, AppError> {
    if !file_is_authorized(connection, &file.file_id)? {
        return Ok(false);
    }
    if !scope.knowledge_space_ids.is_empty()
        && !file_matches_knowledge_space_scope(connection, file, &scope.knowledge_space_ids)?
    {
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
            || (!scope.knowledge_space_ids.is_empty()
                && !file_matches_knowledge_space_scope(
                    connection,
                    file,
                    &scope.knowledge_space_ids,
                )?)
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
        if kind != "rule" {
            let member = connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM collection_memberships WHERE collection_id = ?1 AND file_id = ?2 AND state = 'active')",
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
    let candidates =
        search_semantic_usearch_candidates(connection, query, scoped_file_ids)?.unwrap_or(
            search_semantic_exact_candidates(connection, query, scoped_file_ids)?,
        );

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

fn search_semantic_exact_candidates(
    connection: &Connection,
    query: &SemanticQuery<'_>,
    scoped_file_ids: &HashSet<Uuid>,
) -> Result<Vec<SemanticCandidate>, AppError> {
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
    Ok(candidates)
}

fn search_semantic_usearch_candidates(
    connection: &Connection,
    query: &SemanticQuery<'_>,
    scoped_file_ids: &HashSet<Uuid>,
) -> Result<Option<Vec<SemanticCandidate>>, AppError> {
    let active = connection
        .query_row(
            "SELECT generation_id, dimension, index_path, item_count FROM index_generations WHERE model_artifact_id = ?1 AND status = 'active' LIMIT 1",
            [query.model_artifact_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, u32>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, usize>(3)?,
                ))
            },
        )
        .optional()
        .map_err(|error| storage_error("VECTOR_INDEX_GENERATION_QUERY_FAILED", error, true))?;
    let Some((generation_id, dimension, index_path, item_count)) = active else {
        return Ok(None);
    };
    if dimension as usize != query.vector.len() || item_count == 0 {
        return Ok(None);
    }
    let indexed_files: u64 = connection
        .query_row(
            "SELECT COUNT(DISTINCT file_id) FROM vector_index_keys WHERE generation_id = ?1",
            [&generation_id],
            |row| row.get(0),
        )
        .map_err(|error| storage_error("VECTOR_INDEX_KEY_QUERY_FAILED", error, true))?;
    if scoped_file_ids.len() < indexed_files as usize {
        return Ok(None);
    }
    let candidate_count = item_count.clamp(1, 5_000);
    let matches = match crate::search_index(
        std::path::Path::new(&index_path),
        query.vector,
        candidate_count,
    ) {
        Ok(matches) => matches,
        Err(_) => return Ok(None),
    };
    if matches.is_empty() {
        return Ok(None);
    }
    let score_by_key = matches
        .iter()
        .map(|item| (item.key, (1.0 - item.distance / 2.0).clamp(0.0, 1.0)))
        .collect::<HashMap<_, _>>();
    let placeholders = std::iter::repeat_n("?", score_by_key.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT vector_key, chunk_id, file_id FROM vector_index_keys WHERE generation_id = ? AND vector_key IN ({placeholders})"
    );
    let mut values = Vec::<SqlValue>::with_capacity(score_by_key.len() + 1);
    values.push(SqlValue::Text(generation_id));
    for key in score_by_key.keys() {
        let key = i64::try_from(*key).map_err(|_| {
            AppError::new(
                "VECTOR_INDEX_CAPACITY_EXCEEDED",
                "向量键超出数据库范围",
                false,
            )
        })?;
        values.push(SqlValue::Integer(key));
    }
    let mut statement = connection
        .prepare(&sql)
        .map_err(|error| storage_error("VECTOR_INDEX_KEY_QUERY_FAILED", error, true))?;
    let rows = statement
        .query_map(params_from_iter(values), |row| {
            Ok((
                row.get::<_, u64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(|error| storage_error("VECTOR_INDEX_KEY_QUERY_FAILED", error, true))?;
    let mut best_by_file = HashMap::<Uuid, (String, f32)>::new();
    for row in rows {
        let (key, chunk_id, file_id) =
            row.map_err(|error| storage_error("VECTOR_INDEX_KEY_QUERY_FAILED", error, true))?;
        let file_id = parse_uuid_value(&file_id)?;
        if !scoped_file_ids.contains(&file_id) {
            continue;
        }
        let Some(score) = score_by_key.get(&key).copied() else {
            continue;
        };
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
    if candidates.is_empty() {
        Ok(None)
    } else {
        Ok(Some(candidates))
    }
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

fn initial_parse_status(extension: &str) -> &'static str {
    match extension.to_ascii_lowercase().as_str() {
        "pdf" | "docx" | "docm" | "xlsx" | "xlsm" | "pptx" | "pptm" | "csv" | "tsv" | "md"
        | "txt" | "html" | "htm" | "jpg" | "jpeg" | "png" | "tif" | "tiff" | "bmp" | "webp"
        | "doc" | "xls" | "ppt" | "zip" | "rs" | "py" | "js" | "jsx" | "mjs" | "cjs" | "ts"
        | "tsx" | "java" | "kt" | "kts" | "go" | "c" | "cc" | "cpp" | "h" | "hpp" | "cs" | "rb"
        | "php" | "swift" | "scala" | "sh" | "ps1" | "sql" | "json" | "yaml" | "yml" | "toml"
        | "xml" | "css" | "scss" | "vue" | "svelte" => "pending",
        _ => "unsupported",
    }
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
    let initial_parse_status = initial_parse_status(&file.extension);
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
                "INSERT OR IGNORE INTO files (file_id, canonical_path, path_key, name, extension, size_bytes, modified_at, discovered_at, availability, volume_id, display_name, mime_type, fs_created_at, windows_file_id, parse_status, first_seen_at, last_seen_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'present', ?9, ?4, ?10, ?11, ?12, ?13, ?8, ?8)",
                params![file_id.to_string(), file.canonical_path, file.path_key, file.name, file.extension, file.size_bytes, file.modified_at.to_rfc3339(), now.to_rfc3339(), file.volume_id, file.mime_type, file.created_at.map(|value| value.to_rfc3339()), file.windows_file_id, initial_parse_status],
            )
            .map_err(|error| storage_error("FILE_UPSERT_FAILED", error, true))?;
        let revision_id = Uuid::now_v7();
        transaction
            .execute(
                "INSERT INTO file_revisions (revision_id, file_id, size_bytes, fs_modified_at, content_sha256, metadata_fingerprint, created_at, parse_status) VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6, ?7)",
                params![revision_id.to_string(), file_id.to_string(), file.size_bytes, file.modified_at.to_rfc3339(), fingerprint, now.to_rfc3339(), initial_parse_status],
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
            "INSERT INTO files (file_id, canonical_path, path_key, name, extension, size_bytes, modified_at, discovered_at, availability, volume_id, display_name, mime_type, fs_created_at, windows_file_id, current_revision_id, parse_status, first_seen_at, last_seen_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'present', ?9, ?4, ?10, ?11, ?12, ?13, ?14, ?8, ?8) ON CONFLICT(file_id) DO UPDATE SET canonical_path = excluded.canonical_path, path_key = excluded.path_key, name = excluded.name, display_name = excluded.display_name, extension = excluded.extension, mime_type = excluded.mime_type, size_bytes = excluded.size_bytes, fs_created_at = excluded.fs_created_at, modified_at = excluded.modified_at, windows_file_id = excluded.windows_file_id, volume_id = excluded.volume_id, content_sha256 = CASE WHEN files.current_revision_id <> excluded.current_revision_id THEN NULL ELSE files.content_sha256 END, current_revision_id = excluded.current_revision_id, parse_status = CASE WHEN files.current_revision_id <> excluded.current_revision_id THEN excluded.parse_status ELSE files.parse_status END, availability = 'present', last_seen_at = excluded.last_seen_at",
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
                initial_parse_status,
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
    let (user_message, user_action) = map_storage_error(&error, code);
    let mut err = AppError::new(code, user_message, retryable);
    if let Some(action) = user_action {
        err.user_action = Some(action.into());
    }
    let mut details = serde_json::Map::new();
    details.insert("technical".to_owned(), serde_json::Value::String(error.to_string()));
    err.details = Some(Box::new(serde_json::Value::Object(details)));
    err
}

fn map_storage_error(error: &rusqlite::Error, _code: &str) -> (String, Option<&'static str>) {
    let text = error.to_string().to_lowercase();
    if text.contains("database is locked") || text.contains("database locked") {
        ("本地资料库暂时繁忙，请稍后重试".into(), Some("如果频繁出现，可尝试关闭其他同时访问资料的软件"))
    } else if text.contains("disk i/o") || text.contains("disk full") || text.contains("no space") {
        ("磁盘读写出现问题".into(), Some("请检查磁盘空间是否充足，或是否有杀毒软件干扰了拾忆"))
    } else if text.contains("readonly") || text.contains("read-only") || text.contains("permission denied") {
        ("资料库写入权限异常".into(), Some("请检查杀毒软件或系统权限设置，确保拾忆可以正常写入应用数据"))
    } else if text.contains("no such table") || text.contains("no such column") {
        ("资料库结构需要升级，重启拾忆即可完成自动迁移".into(), None)
    } else if text.contains("unable to open") || text.contains("cannot open") {
        ("无法打开资料库文件".into(), Some("请确认拾忆应用数据目录未被移动或删除，重启后会自动修复"))
    } else if text.contains("malformed") || text.contains("corrupt") || text.contains("not a database") {
        ("资料库文件损坏".into(), Some("请在设置中重建索引，源文件不会受到影响"))
    } else if text.contains("busy") || text.contains("timeout") {
        ("资料库操作超时，请稍后重试".into(), None)
    } else {
        ("资料库操作失败，请重启拾忆后重试".into(), Some("如仍未解决，可在设置中导出诊断信息"))
    }
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

    #[test]
    fn metadata_only_formats_never_enter_the_content_parser_queue() {
        for extension in ["exe", "dll", "msi", "mp4", "mp3", "7z", "unknown"] {
            assert_eq!(initial_parse_status(extension), "unsupported");
        }
        for extension in ["pdf", "docx", "png", "zip", "rs", "py", "tsx"] {
            assert_eq!(initial_parse_status(extension), "pending");
        }
    }

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
                (7, "persistent_ask_sessions".to_owned()),
                (8, "ai_collection_suggestions".to_owned()),
                (9, "usearch_index_generations".to_owned()),
                (10, "multimodal_image_assets".to_owned()),
                (11, "metadata_only_file_policy".to_owned()),
                (12, "recoverable_image_understanding".to_owned()),
                (13, "knowledge_spaces".to_owned()),
                (14, "application_settings".to_owned()),
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
        assert_eq!(store.storage_quota_override().expect("default quota"), None);
        let quota = 12 * 1024 * 1024 * 1024;
        assert_eq!(
            store.set_storage_quota_override(quota).expect("set quota"),
            quota
        );
        assert_eq!(
            store.storage_quota_override().expect("saved quota"),
            Some(quota)
        );
        assert_eq!(
            store.set_storage_quota_override(0).unwrap_err().code,
            "STORAGE_POLICY_INVALID"
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
        assert_eq!(
            store.migration_history().expect("history").len(),
            CURRENT_SCHEMA_VERSION as usize
        );
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
        let space = store
            .create_knowledge_space(&KnowledgeSpaceRequest {
                name: "归航知识空间".into(),
                description: Some("只在拾忆中组合资料范围".into()),
                root_ids: vec![],
                collection_ids: vec![manual.collection_id],
            })
            .expect("create knowledge space");
        assert_eq!(space.file_count, 1);
        let scoped = store
            .search(&crate::SearchRequest {
                query: inbox.items[0].display_name.clone(),
                scope: ScopeFilter {
                    knowledge_space_ids: vec![space.space_id],
                    root_ids: vec![],
                    collection_ids: vec![],
                    file_ids: vec![],
                    extensions: vec![],
                    modified_from: None,
                    modified_to: None,
                    availability: crate::Availability::Present,
                },
                mode: SearchMode::Filename,
                sort: crate::SearchSort::Relevance,
                page_size: 20,
                cursor: None,
            })
            .expect("search knowledge space");
        assert_eq!(scoped.results.len(), 1);
        let updated_space = store
            .update_knowledge_space(
                &space.space_id,
                &KnowledgeSpaceRequest {
                    name: "全部归航资料".into(),
                    description: None,
                    root_ids: vec![root.root_id],
                    collection_ids: vec![],
                },
            )
            .expect("update knowledge space");
        assert_eq!(updated_space.file_count, 2);
        assert_eq!(store.list_knowledge_spaces().expect("list spaces").len(), 1);
        let filtered = store
            .create_collection(&CreateCollectionRequest {
                name: "有效归航文本".into(),
                description: Some("验证大小和硬排除条件".into()),
                icon: "sparkles".into(),
                color: "#71a7ca".into(),
                kind: CollectionKind::Rule,
                rule: Some(CollectionRule {
                    operator: crate::RuleOperator::All,
                    extensions: vec!["txt".into()],
                    filename_keywords: vec![],
                    path_keywords: vec![],
                    text_keywords: vec![],
                    parse_statuses: vec![],
                    modified_within_days: None,
                    min_size_bytes: Some(1),
                    max_size_bytes: Some(1024),
                    exclude_extensions: vec![],
                    exclude_filename_keywords: vec!["v2".into()],
                    exclude_path_keywords: vec![],
                    exclude_text_keywords: vec![],
                }),
            })
            .expect("create filtered rule collection");
        let filtered_files = store
            .collection_files(&filtered.collection_id)
            .expect("query filtered collection");
        assert_eq!(filtered_files.len(), 1);
        assert_eq!(filtered_files[0].display_name, "归航计划-最终版.txt");

        let refresh = store
            .refresh_file_relations(100)
            .expect("refresh relations");
        assert_eq!(refresh.hashed_files, 2);
        assert_eq!(refresh.exact_duplicate_pairs, 1);
        assert_eq!(refresh.version_candidate_pairs, 1);
        let relation_inbox = store
            .query_inbox(&InboxQuery {
                status: TriageStatus::New,
                event_types: vec![InboxEventType::RelationSuggested],
                root_ids: vec![root.root_id],
                date_from: None,
                date_to: None,
                cursor: None,
                page_size: 20,
            })
            .expect("query relation inbox events");
        assert_eq!(relation_inbox.items.len(), 2);
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
        store
            .delete_knowledge_space(&space.space_id)
            .expect("delete knowledge space");
        assert!(
            store
                .list_knowledge_spaces()
                .expect("empty spaces")
                .is_empty()
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

        let asset_id = Uuid::now_v7();
        let asset_directory = directory
            .path()
            .join("image-assets")
            .join(revision_id.to_string());
        fs::create_dir_all(&asset_directory).expect("create asset cache");
        let asset_path = asset_directory.join(format!("{asset_id}.png"));
        fs::write(&asset_path, b"read-only-derived-image").expect("write derived image");
        let asset_hash = hash_file_sha256(&asset_path).expect("hash derived image");
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
            image_assets: vec![ImageAsset {
                asset_id,
                revision_id,
                asset_kind: "embedded_image".into(),
                cache_path: asset_path.to_string_lossy().into_owned(),
                mime_type: "image/png".into(),
                size_bytes: b"read-only-derived-image".len() as u64,
                sha256: asset_hash,
                locator: SourceLocator {
                    kind: crate::SourceKind::Docx,
                    paragraph_no: Some(3),
                    ..SourceLocator::default()
                },
                ocr_text: None,
                description: None,
                vision_model_id: None,
                status: "pending_understanding".into(),
                error: None,
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
        let generation = store
            .rebuild_vector_generation("embedding-test", 2)
            .expect("build USearch generation");
        assert_eq!(generation.status, "active");
        assert_eq!(generation.item_count, 1);
        assert_eq!(generation.coverage, 1.0);
        assert_eq!(
            store
                .active_vector_generation("embedding-test")
                .expect("read active generation")
                .map(|item| item.generation_id),
            Some(generation.generation_id)
        );
        let semantic_scope = ScopeFilter {
            knowledge_space_ids: vec![],
            root_ids: vec![root.root_id],
            collection_ids: vec![],
            file_ids: vec![],
            extensions: vec![],
            modified_from: None,
            modified_to: None,
            availability: crate::Availability::Present,
        };
        assert_eq!(
            store
                .semantic_index_coverage(&semantic_scope, "embedding-test")
                .expect("old model coverage"),
            (1.0, 1.0)
        );
        assert_eq!(
            store
                .semantic_index_coverage(&semantic_scope, "embedding-next")
                .expect("new model must not inherit old coverage"),
            (0.0, 0.0)
        );
        store
            .commit_chunk_embeddings(
                "embedding-next",
                3,
                &[ChunkEmbeddingInput {
                    chunk_id: pending_embeddings[0].chunk_id,
                    vector: vec![0.0, 1.0, 0.0],
                }],
            )
            .expect("commit next embedding generation");
        let next_generation = store
            .rebuild_vector_generation("embedding-next", 3)
            .expect("build next USearch generation");
        assert_eq!(next_generation.status, "active");
        assert_eq!(next_generation.dimension, 3);
        assert_eq!(
            store
                .active_vector_generation("embedding-test")
                .expect("old generation still queryable")
                .map(|item| item.generation_id),
            Some(generation.generation_id)
        );

        let request = crate::SearchRequest {
            query: "混合召回".into(),
            scope: semantic_scope,
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
        assert_eq!(preview.image_assets.len(), 1);
        assert_eq!(preview.image_assets[0].asset_id, asset_id);
        assert!(
            !serde_json::to_value(&preview.image_assets[0])
                .expect("serialize public image asset")
                .as_object()
                .expect("image asset object")
                .contains_key("cache_path")
        );
        assert_eq!(preview.nodes[0].locator.line_start, Some(8));
        assert!(!preview.truncated);
        let (authorized_asset_path, authorized_asset_mime, authorized_asset_size) = store
            .authorized_image_asset_path(&asset_id)
            .expect("authorized image asset");
        assert_eq!(authorized_asset_path, asset_path);
        assert_eq!(authorized_asset_mime, "image/png");
        assert_eq!(
            authorized_asset_size,
            b"read-only-derived-image".len() as u64
        );
        assert_eq!(
            store
                .image_understanding_stats()
                .expect("image understanding stats"),
            (1, 0, 1)
        );
        let first_claim = store
            .claim_pending_image_understanding("vision-test")
            .expect("claim image understanding")
            .expect("pending image");
        assert_eq!(first_claim.asset_id, asset_id);
        assert_eq!(first_claim.attempt_count, 1);
        assert_eq!(
            store
                .recover_interrupted_image_understanding()
                .expect("recover image understanding"),
            1
        );
        let recovered_claim = store
            .claim_pending_image_understanding("vision-test")
            .expect("claim recovered image understanding")
            .expect("recovered pending image");
        assert_eq!(recovered_claim.asset_id, asset_id);
        assert_eq!(recovered_claim.attempt_count, 2);
        let image_result = ImageUnderstandingResult {
            asset_id,
            revision_id,
            model_artifact_id: "vision-test".into(),
            summary: "柱状图显示第二季度收入明显增长。".into(),
            visible_text: Some("第二季度 收入 128 万元".into()),
            keywords: vec!["季度收入".into(), "增长".into()],
            entities: vec!["第二季度".into()],
            chart_summary: Some("第二季度柱高于第一季度。".into()),
            idempotency_key: recovered_claim.idempotency_key,
        };
        store
            .commit_image_understanding(&image_result)
            .expect("commit image understanding");
        store
            .commit_image_understanding(&image_result)
            .expect("idempotent image understanding commit");
        assert_eq!(
            store
                .image_understanding_stats()
                .expect("completed image understanding stats"),
            (1, 1, 0)
        );
        let image_preview = store
            .file_preview(&file_id, 10)
            .expect("preview image description");
        assert_eq!(image_preview.nodes.len(), 2);
        assert_eq!(image_preview.image_assets[0].status, "ready");
        assert_eq!(
            image_preview.image_assets[0].description.as_deref(),
            Some("柱状图显示第二季度收入明显增长。")
        );
        let ask = AskRequest {
            question: "第二季度收入".into(),
            session_id: None,
            scope: request.scope.clone(),
            answer_style: crate::AnswerStyle::Concise,
            retrieval_limit: 12,
            max_source_files: 8,
            strict_evidence: true,
            mode: crate::AskMode::EvidenceExtracts,
            allow_degraded_extractive: true,
        };
        let image_answer = store
            .answer_extractively(&ask, None)
            .expect("retrieve image description");
        assert_eq!(
            image_answer.claims[0].citations[0].image_asset_id,
            Some(asset_id)
        );
        store
            .validate_answer_evidence(&image_answer)
            .expect("validate image evidence");
        assert_eq!(
            store
                .list_pending_embedding_chunks("embedding-test", 20)
                .expect("image description embedding work")
                .len(),
            1
        );
        fs::write(&asset_path, b"tampered-derived-image!").expect("tamper derived cache");
        assert_eq!(
            store
                .authorized_image_asset_path(&asset_id)
                .expect_err("tampered image cache must not be served")
                .code,
            "IMAGE_ASSET_UNAVAILABLE"
        );
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
        assert_eq!(
            store
                .authorized_image_asset_path(&asset_id)
                .expect_err("disabled roots cannot preview derived images")
                .code,
            "IMAGE_ASSET_NOT_FOUND"
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
                knowledge_space_ids: vec![],
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
                knowledge_space_ids: vec![],
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
