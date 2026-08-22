use std::collections::{BTreeMap, HashMap, HashSet};
use std::{
    fs,
    fs::File,
    io::Read,
    path::PathBuf,
    sync::{Arc, Condvar, Mutex, OnceLock},
    thread,
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use regex::Regex;
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OptionalExtension, Transaction, params, params_from_iter};
use sha2::{Digest, Sha256};
use uuid::Uuid;

#[cfg(test)]
use crate::memory::MemoryKind;
use crate::memory::{
    MemoryAlias, MemoryEntity, MemoryRelation, MemorySource, MemoryStatus, MemoryTargetType,
    MemoryWriteInput, normalize_alias,
};
use crate::organizing::{RelationEdge, SeedProfile, seed_expand_semantic_groups};
use crate::profile_builder::{
    build_representative_text, extract_section_titles, pick_head_mid_tail, representative_text_hash,
};
use crate::{
    AnswerResult, AnswerSourceFile, AppError, AppLogRecord, AskMessage, AskMessagePage, AskRequest,
    AskSessionContext, AskSessionPage, AskSessionSummary, AuthorizationSource,
    BUILT_IN_EXCLUSION_RULES, CandidateRoot, CandidateRootStatus, CandidateRootType,
    ChunkEmbeddingInput, CollectionKind, CollectionModelReview, CollectionRecord, CollectionRule,
    CollectionSuggestedMember, CollectionSuggestion, CollectionSuggestionPage,
    CollectionSuggestionQuery, CollectionSuggestionRefreshResult,
    CollectionSuggestionUpdateRequest, CreateCollectionRequest, DegradationLevel, DegradationState,
    DiscoveredFile, DocumentNode, DocumentProfile, DocumentType, EvaluationCaseRecord,
    EvaluationIntegritySnapshot, EvaluationResultRecord, EvaluationRunRecord, ExclusionRule,
    ExclusionRuleClass, ExclusionRuleInput, ExclusionRuleType, FilePage, FileProcessingDisposition,
    FileQuery, FileRecord, FileRelation, FileSystemEvent, HealthCheckItem, ImageAsset,
    ImageOcrResult, ImageUnderstandingResult, InboxEventType, InboxItem, InboxPage, InboxQuery,
    InboxUpdateRequest, IndexActivityStats, IndexRebuildResult, JobRecord, JobStatus, LogPage,
    LogQuery, MaintenanceSnapshot, NodeTracePage, NodeTraceQuery, NodeTraceRecord,
    OperationTraceInput, OperationTracePage, OperationTraceQuery, OperationTraceRecord,
    ParseOutcome, ParseResult, ParseStatus, PendingEmbeddingChunk, PendingImageOcr,
    PendingImageUnderstanding, ProcessingCoverageSnapshot, ProfileRefreshResult, RankedHit,
    RelationGroupMemberRecord, RelationGroupPage, RelationGroupQuery, RelationGroupRecord,
    RelationGroupRole, RelationGroupType, RelationPage, RelationQuery, RelationRefreshResult,
    RelationType, ResolutionStatus, RootKind, RootRecord, RootSource, RootStatus, ScanOutcome,
    ScopeFilter, SearchMode, SemanticQuery, SourceLocator, TraceNodeInput, TraceNodeMeta,
    TriageStatus, VolumeType, WatchMode, chunks_from_nodes, cluster_relation_edges, fts_query,
    normalized_version_key,
};

pub const CURRENT_SCHEMA_VERSION: u32 = 34;

/// operation_traces 表的 schema 版本（与 CURRENT_SCHEMA_VERSION 独立演进）。
pub const OPERATION_TRACE_SCHEMA_VERSION: u32 = 1;

type VectorSourceRow = (u64, String, String, String, Vec<f32>);
type SemanticCandidate = (Uuid, String, f32);

struct Migration {
    version: u32,
    name: &'static str,
    sql: &'static str,
}

const ROOT_SELECT: &str = "SELECT root_id, path, canonical_path, path_key, root_file_id, volume_id, volume_type, authorization_source, root_kind, label, enabled, status, watch_mode, coverage_parent_root_id, file_count, permission_error_count, last_scan_at, COALESCE((SELECT COUNT(DISTINCT k.file_id) FROM vector_index_keys k JOIN index_generations g ON g.generation_id = k.generation_id AND g.status = 'active' JOIN chunks c ON c.chunk_id = k.chunk_id AND c.file_id = k.file_id AND c.revision_id = k.revision_id JOIN files f ON f.file_id = k.file_id WHERE f.current_revision_id = k.revision_id AND f.availability = 'present' AND EXISTS (SELECT 1 FROM file_root_memberships m WHERE m.root_id = roots.root_id AND m.file_id = f.file_id)), 0) AS indexed_file_count, COALESCE((SELECT COUNT(*) FROM file_root_memberships m JOIN files f ON f.file_id = m.file_id WHERE m.root_id = roots.root_id AND f.availability = 'present' AND f.processing_disposition IN ('parseable_content','image_ocr','read_only_text','archive_manifest')), 0) AS indexable_file_count, COALESCE((SELECT COUNT(*) FROM file_root_memberships m JOIN files f ON f.file_id = m.file_id WHERE m.root_id = roots.root_id AND f.availability = 'present' AND f.parse_status = 'parsed' AND f.processing_disposition IN ('parseable_content','image_ocr','read_only_text','archive_manifest')), 0) AS parsed_file_count, COALESCE((SELECT COUNT(DISTINCT e.file_id) FROM chunk_embeddings e JOIN chunks c ON c.chunk_id = e.chunk_id AND c.file_id = e.file_id AND c.revision_id = e.revision_id JOIN files f ON f.file_id = e.file_id WHERE f.current_revision_id = e.revision_id AND f.availability = 'present' AND EXISTS (SELECT 1 FROM file_root_memberships m WHERE m.root_id = roots.root_id AND m.file_id = f.file_id)), 0) AS embedded_file_count, COALESCE((SELECT COUNT(DISTINCT k.file_id) FROM vector_index_keys k JOIN index_generations g ON g.generation_id = k.generation_id AND g.status = 'active' JOIN chunks c ON c.chunk_id = k.chunk_id AND c.file_id = k.file_id AND c.revision_id = k.revision_id JOIN files f ON f.file_id = k.file_id WHERE f.current_revision_id = k.revision_id AND f.availability = 'present' AND EXISTS (SELECT 1 FROM file_root_memberships m WHERE m.root_id = roots.root_id AND m.file_id = f.file_id)), 0) AS active_index_file_count FROM roots";

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
            CREATE TABLE IF NOT EXISTS degradation_states (
                state_id INTEGER PRIMARY KEY CHECK (state_id = 1),
                state_json TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
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
        name: "knowledge_spaces_removed",
        sql: r#"
            DROP TABLE IF EXISTS knowledge_space_collections;
            DROP TABLE IF EXISTS knowledge_space_roots;
            DROP TABLE IF EXISTS knowledge_spaces;
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
    Migration {
        version: 15,
        name: "relation_evidence_versions",
        sql: r#"
            ALTER TABLE file_relations ADD COLUMN algorithm_version TEXT;
            ALTER TABLE file_relations ADD COLUMN model_version TEXT;
            ALTER TABLE file_relations ADD COLUMN left_revision_id TEXT;
            ALTER TABLE file_relations ADD COLUMN right_revision_id TEXT;
            CREATE INDEX idx_file_relations_model_algorithm
                ON file_relations(relation_type, model_version, algorithm_version, review_status);
        "#,
    },
    Migration {
        version: 16,
        name: "stable_keyset_paging_and_scan_checkpoints",
        sql: r#"
            CREATE INDEX IF NOT EXISTS idx_files_keyset_page
                ON files(last_seen_at DESC, file_id DESC);
            CREATE INDEX IF NOT EXISTS idx_inbox_keyset_page
                ON inbox_events(triage_status, observed_at DESC, inbox_id DESC);
            CREATE INDEX IF NOT EXISTS idx_memberships_root_file
                ON file_root_memberships(root_id, file_id);
            CREATE TABLE IF NOT EXISTS scan_seen_memberships (
                job_id TEXT NOT NULL,
                root_id TEXT NOT NULL,
                file_id TEXT NOT NULL,
                PRIMARY KEY (job_id, root_id, file_id)
            );
            CREATE INDEX IF NOT EXISTS idx_scan_seen_root
                ON scan_seen_memberships(job_id, root_id, file_id);
        "#,
    },
    Migration {
        version: 17,
        name: "inbox_triage_resolution_split",
        sql: r#"
            ALTER TABLE inbox_events ADD COLUMN resolution_status TEXT NOT NULL DEFAULT 'normal';
            ALTER TABLE inbox_events ADD COLUMN attempt_count INTEGER NOT NULL DEFAULT 0;
            ALTER TABLE inbox_events ADD COLUMN last_attempt_at TEXT;
            ALTER TABLE inbox_events ADD COLUMN last_error_json TEXT;
            ALTER TABLE ask_sessions ADD COLUMN title TEXT;
            ALTER TABLE ask_sessions ADD COLUMN last_error_json TEXT;
            ALTER TABLE ask_messages ADD COLUMN error_json TEXT;

            UPDATE inbox_events
                SET resolution_status = 'abandoned'
                WHERE triage_status = 'ignored'
                  AND (event_type IN ('parse_failed', 'ocr_required') OR error_code IS NOT NULL);
            UPDATE inbox_events
                SET resolution_status = 'resolved'
                WHERE triage_status <> 'ignored'
                  AND (event_type IN ('parse_failed', 'ocr_required') OR error_code IS NOT NULL)
                  AND EXISTS (
                      SELECT 1 FROM files f
                      WHERE f.file_id = inbox_events.file_id
                        AND f.parse_status = 'parsed'
                  );
            UPDATE inbox_events
                SET resolution_status = 'pending_retry'
                WHERE triage_status <> 'ignored'
                  AND (event_type IN ('parse_failed', 'ocr_required') OR error_code IS NOT NULL)
                  AND resolution_status = 'normal';
            UPDATE inbox_events SET triage_status = 'new' WHERE triage_status = 'error';

            CREATE INDEX idx_inbox_resolution_time
                ON inbox_events(resolution_status, observed_at DESC, inbox_id DESC);
        "#,
    },
    Migration {
        version: 18,
        name: "remove_knowledge_spaces",
        sql: r#"
            DROP TABLE IF EXISTS knowledge_space_collections;
            DROP TABLE IF EXISTS knowledge_space_roots;
            DROP TABLE IF EXISTS knowledge_spaces;
        "#,
    },
    Migration {
        version: 19,
        name: "local_ai_runtime_and_media_transcripts",
        sql: r#"
            CREATE TABLE runtime_task_checkpoints (
                operation_id TEXT PRIMARY KEY,
                task_kind TEXT NOT NULL,
                backend TEXT NOT NULL,
                model_id TEXT,
                idempotency_key TEXT,
                state TEXT NOT NULL,
                priority INTEGER NOT NULL,
                checkpoint_json TEXT,
                error_json TEXT,
                created_at TEXT NOT NULL,
                started_at TEXT,
                updated_at TEXT NOT NULL,
                finished_at TEXT
            );
            CREATE UNIQUE INDEX idx_runtime_task_idempotency
                ON runtime_task_checkpoints(idempotency_key)
                WHERE idempotency_key IS NOT NULL;
            CREATE INDEX idx_runtime_task_queue
                ON runtime_task_checkpoints(state, priority, created_at);

            CREATE TABLE runtime_instances (
                instance_id TEXT PRIMARY KEY,
                backend TEXT NOT NULL,
                model_id TEXT,
                device TEXT NOT NULL,
                state TEXT NOT NULL,
                memory_bytes INTEGER NOT NULL DEFAULT 0,
                gpu_memory_bytes INTEGER NOT NULL DEFAULT 0,
                loaded_at TEXT NOT NULL,
                last_used_at TEXT NOT NULL,
                unloaded_at TEXT,
                error_json TEXT
            );
            CREATE INDEX idx_runtime_instances_state
                ON runtime_instances(backend, state, last_used_at);

            CREATE TABLE media_transcripts (
                transcript_id TEXT PRIMARY KEY,
                file_id TEXT NOT NULL REFERENCES files(file_id) ON DELETE CASCADE,
                revision_id TEXT NOT NULL REFERENCES file_revisions(revision_id) ON DELETE CASCADE,
                model_artifact_id TEXT NOT NULL,
                language TEXT,
                duration_ms INTEGER NOT NULL,
                status TEXT NOT NULL,
                source_sha256 TEXT NOT NULL,
                error_json TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                UNIQUE(file_id, revision_id, model_artifact_id)
            );
            CREATE INDEX idx_media_transcripts_status
                ON media_transcripts(status, updated_at);

            CREATE TABLE transcript_segments (
                segment_id TEXT PRIMARY KEY,
                transcript_id TEXT NOT NULL REFERENCES media_transcripts(transcript_id) ON DELETE CASCADE,
                ordinal INTEGER NOT NULL,
                start_ms INTEGER NOT NULL,
                end_ms INTEGER NOT NULL,
                text TEXT NOT NULL,
                confidence REAL,
                chunk_id TEXT REFERENCES chunks(chunk_id) ON DELETE SET NULL,
                created_at TEXT NOT NULL,
                UNIQUE(transcript_id, ordinal)
            );
            CREATE INDEX idx_transcript_segments_time
                ON transcript_segments(transcript_id, start_ms, end_ms);
        "#,
    },
    Migration {
        version: 20,
        name: "observable_ocr_attempts",
        sql: r#"
            CREATE TABLE ocr_attempts (
                attempt_id TEXT PRIMARY KEY,
                revision_id TEXT NOT NULL REFERENCES file_revisions(revision_id) ON DELETE CASCADE,
                ordinal INTEGER NOT NULL,
                engine TEXT NOT NULL,
                model_version TEXT,
                status TEXT NOT NULL,
                page_no INTEGER,
                confidence REAL,
                fallback_reason TEXT,
                elapsed_ms INTEGER NOT NULL,
                error_json TEXT,
                created_at TEXT NOT NULL,
                UNIQUE(revision_id, ordinal)
            );
            CREATE INDEX idx_ocr_attempts_revision
                ON ocr_attempts(revision_id, ordinal);
        "#,
    },
    Migration {
        version: 21,
        name: "recoverable_processing_and_scan_checkpoints",
        sql: r#"
            ALTER TABLE files ADD COLUMN processing_disposition TEXT NOT NULL DEFAULT 'unknown';
            ALTER TABLE files ADD COLUMN processing_reason_code TEXT;
            ALTER TABLE files ADD COLUMN detected_mime_type TEXT;
            ALTER TABLE jobs ADD COLUMN checkpoint_json TEXT;

            CREATE TABLE processing_attempts (
                attempt_id TEXT PRIMARY KEY,
                file_id TEXT REFERENCES files(file_id) ON DELETE CASCADE,
                revision_id TEXT REFERENCES file_revisions(revision_id) ON DELETE CASCADE,
                operation TEXT NOT NULL,
                engine TEXT,
                model_version TEXT,
                status TEXT NOT NULL,
                attempt_no INTEGER NOT NULL,
                elapsed_ms INTEGER NOT NULL,
                retryable INTEGER NOT NULL,
                error_json TEXT,
                created_at TEXT NOT NULL
            );
            CREATE INDEX idx_processing_attempts_file_operation
                ON processing_attempts(file_id, operation, created_at DESC);

            CREATE TABLE scan_checkpoints (
                job_id TEXT PRIMARY KEY REFERENCES jobs(job_id) ON DELETE CASCADE,
                root_id TEXT NOT NULL REFERENCES roots(root_id) ON DELETE CASCADE,
                batch_no INTEGER NOT NULL,
                enumerated_items INTEGER NOT NULL,
                committed_items INTEGER NOT NULL,
                isolated_failures INTEGER NOT NULL,
                retry_count INTEGER NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE INDEX idx_scan_checkpoints_root
                ON scan_checkpoints(root_id, updated_at DESC);

            UPDATE files
               SET processing_disposition = CASE
                   WHEN extension IN ('jpg','jpeg','png','tif','tiff','bmp','webp') THEN 'image_ocr'
                   WHEN extension IN ('exe','dll','msi') THEN 'safe_metadata'
                   WHEN extension IN ('mp3','wav','m4a','flac','mp4','mkv','mov','avi') THEN 'media_metadata'
                   WHEN extension IN ('zip','rar','7z','tar','gz') THEN 'archive_manifest'
                   WHEN parse_status = 'unsupported' THEN 'capability_missing'
                   ELSE 'parseable_content'
               END;
        "#,
    },
    Migration {
        version: 22,
        name: "node_traces",
        sql: r#"
            CREATE TABLE node_traces (
                trace_id TEXT PRIMARY KEY,
                flow TEXT NOT NULL,
                node TEXT NOT NULL,
                correlation_id TEXT NOT NULL,
                session_id TEXT,
                entity_id TEXT,
                input_json TEXT NOT NULL,
                output_json TEXT NOT NULL,
                status TEXT NOT NULL,
                elapsed_ms INTEGER,
                created_at TEXT NOT NULL
            );
            CREATE INDEX idx_node_traces_created ON node_traces(created_at DESC, trace_id DESC);
            CREATE INDEX idx_node_traces_correlation ON node_traces(correlation_id);
            CREATE INDEX idx_node_traces_flow ON node_traces(flow, node);
        "#,
    },
    Migration {
        version: 23,
        name: "chunk_neighbor_context_index",
        sql: r#"
            CREATE INDEX IF NOT EXISTS idx_chunks_node_ordinal
                ON chunks(node_id, ordinal);
        "#,
    },
    Migration {
        version: 24,
        name: "relation_groups",
        sql: r#"
            CREATE TABLE IF NOT EXISTS relation_groups (
                group_id TEXT PRIMARY KEY,
                group_type TEXT NOT NULL,
                title TEXT NOT NULL,
                confidence REAL NOT NULL,
                member_count INTEGER NOT NULL,
                review_status TEXT NOT NULL DEFAULT 'suggested',
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_relation_groups_review
                ON relation_groups(review_status, updated_at DESC, group_id DESC);
            CREATE TABLE IF NOT EXISTS relation_group_members (
                group_id TEXT NOT NULL,
                file_id TEXT NOT NULL,
                role TEXT NOT NULL DEFAULT 'member',
                PRIMARY KEY (group_id, file_id)
            );
            CREATE INDEX IF NOT EXISTS idx_relation_group_members_file
                ON relation_group_members(file_id);
        "#,
    },
    Migration {
        version: 25,
        name: "encrypted_disposition_consistency",
        sql: r#"
            UPDATE files
               SET processing_disposition = 'encrypted_or_damaged'
             WHERE parse_status = 'encrypted'
               AND processing_disposition = 'parseable_content';
        "#,
    },
    Migration {
        version: 26,
        name: "ask_session_context",
        sql: r#"
            CREATE TABLE IF NOT EXISTS ask_session_context (
                session_id TEXT PRIMARY KEY,
                active_file_id TEXT,
                active_file_ids_json TEXT NOT NULL DEFAULT '[]',
                active_document_type TEXT,
                active_entity_id TEXT,
                active_collection_id TEXT,
                last_referenced_file_ids_json TEXT NOT NULL DEFAULT '[]',
                last_intent TEXT,
                updated_at TEXT NOT NULL
            );
        "#,
    },
    Migration {
        version: 27,
        name: "document_profiles_classifier_columns",
        sql: r#"
            ALTER TABLE document_profiles ADD COLUMN document_type TEXT;
            ALTER TABLE document_profiles ADD COLUMN type_confidence REAL;
            ALTER TABLE document_profiles ADD COLUMN section_titles_json TEXT NOT NULL DEFAULT '[]';
            ALTER TABLE document_profiles ADD COLUMN representative_text_hash TEXT;
            CREATE INDEX IF NOT EXISTS idx_document_profiles_type
                ON document_profiles(document_type) WHERE document_type IS NOT NULL;
        "#,
    },
    Migration {
        version: 28,
        name: "memory_layer",
        sql: r#"
            CREATE TABLE IF NOT EXISTS memory_entities (
                entity_id TEXT PRIMARY KEY,
                entity_type TEXT NOT NULL,
                canonical_name TEXT NOT NULL,
                metadata_json TEXT NOT NULL DEFAULT '{}',
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                UNIQUE (entity_type, canonical_name)
            );

            CREATE TABLE IF NOT EXISTS memory_relations (
                relation_id TEXT PRIMARY KEY,
                subject_type TEXT NOT NULL,
                subject_id TEXT NOT NULL,
                predicate TEXT NOT NULL,
                object_type TEXT NOT NULL,
                object_id TEXT NOT NULL,
                confidence REAL NOT NULL DEFAULT 0.0,
                status TEXT NOT NULL DEFAULT 'candidate',
                source_type TEXT NOT NULL,
                source_id TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                UNIQUE (subject_type, subject_id, predicate, object_type, object_id)
            );
            CREATE INDEX IF NOT EXISTS idx_memory_relations_subject
                ON memory_relations(subject_type, subject_id);
            CREATE INDEX IF NOT EXISTS idx_memory_relations_object
                ON memory_relations(object_type, object_id);
            CREATE INDEX IF NOT EXISTS idx_memory_relations_status
                ON memory_relations(status);

            CREATE TABLE IF NOT EXISTS memory_aliases (
                alias_id TEXT PRIMARY KEY,
                alias TEXT NOT NULL,
                target_type TEXT NOT NULL,
                target_id TEXT NOT NULL,
                confidence REAL NOT NULL DEFAULT 0.0,
                source_type TEXT NOT NULL,
                source_id TEXT,
                hit_count INTEGER NOT NULL DEFAULT 0,
                last_used_at TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                UNIQUE (alias, target_type, target_id)
            );
            CREATE INDEX IF NOT EXISTS idx_memory_aliases_alias
                ON memory_aliases(alias);
        "#,
    },
    Migration {
        version: 29,
        name: "ask_session_clarification",
        sql: "ALTER TABLE ask_session_context \
              ADD COLUMN pending_clarification_reference TEXT",
    },
    Migration {
        version: 30,
        name: "memory_alias_status",
        // Phase 4.2：别名补 status 列（与 memory_relations 同语义），支持
        // 「待确认的记忆」confirm/reject。回填规则：已存在的可信来源别名
        // （user_explicit / user_confirmed / user_selection / repeated_usage）
        // 视为 confirmed；推断类保持 candidate 等待用户确认。
        sql: "ALTER TABLE memory_aliases ADD COLUMN status TEXT NOT NULL DEFAULT 'candidate'; \
              UPDATE memory_aliases SET status = 'confirmed' WHERE source_type IN \
              ('user_explicit', 'user_confirmed', 'user_selection', 'repeated_usage');",
    },
    Migration {
        version: 31,
        name: "purge_deleted_session_residue",
        // 会话删除一致性：ask_session_context / node_traces 无外键，历史上
        // 删除会话后残留孤儿行（含已删对话的完整问题、回答与引用原文）。
        // 本迁移一次性清理全部孤儿；此后 delete_ask_session 在事务内同步
        // 删除四张关联表，不再产生新孤儿。
        sql: "DELETE FROM ask_messages WHERE session_id NOT IN (SELECT session_id FROM ask_sessions); \
              DELETE FROM ask_session_context WHERE session_id NOT IN (SELECT session_id FROM ask_sessions); \
              DELETE FROM node_traces WHERE session_id IS NOT NULL \
                AND session_id NOT IN (SELECT session_id FROM ask_sessions);",
    },
    Migration {
        version: 32,
        name: "operation_trace_infrastructure",
        // 全链路可观测：新增操作级 operation_traces（一条链路一条），并为
        // 节点级 node_traces 补评测/优化/设备字段。所有新列可空，存量行
        // 不受影响，现有 record_node_trace 写入继续兼容。
        sql: "CREATE TABLE IF NOT EXISTS operation_traces (
                operation_id TEXT PRIMARY KEY,
                correlation_id TEXT NOT NULL,
                session_id TEXT,
                feature_type TEXT NOT NULL,
                request TEXT NOT NULL,
                preset_id TEXT,
                status TEXT NOT NULL,
                created_at TEXT NOT NULL,
                completed_at TEXT,
                total_duration_ms INTEGER,
                schema_version INTEGER NOT NULL
              );
              CREATE INDEX idx_operation_traces_created ON operation_traces(created_at DESC, operation_id DESC);
              CREATE INDEX idx_operation_traces_feature ON operation_traces(feature_type, created_at DESC);
              CREATE INDEX idx_operation_traces_correlation ON operation_traces(correlation_id);
              ALTER TABLE node_traces ADD COLUMN operation_id TEXT;
              ALTER TABLE node_traces ADD COLUMN evaluation_case_id TEXT;
              ALTER TABLE node_traces ADD COLUMN optimization_round INTEGER;
              ALTER TABLE node_traces ADD COLUMN model_id TEXT;
              ALTER TABLE node_traces ADD COLUMN requested_device TEXT;
              ALTER TABLE node_traces ADD COLUMN actual_device TEXT;",
    },
    Migration {
        version: 33,
        name: "evaluation_loop_persistence",
        // 评测闭环持久化：用例、运行、逐例结果三张表。用于「真实资料生成
        // 测试集 → Baseline → Failure Analysis → 优化轮次 → 回归」的闭环，
        // 与既有 evaluation_root 下的加密快照/scorecard 互补：快照提供隔离
        // 只读副本，这里提供可查询的逐例 Gold/预测/诊断记录。
        // EvaluationCase：case_id 为主键；feature_type ∈ SEARCH/ASK/
        // SMART_COLLECTION/FILE_RELATION；split ∈ DEV/HOLDOUT。
        sql: "CREATE TABLE IF NOT EXISTS evaluation_cases (
                case_id TEXT PRIMARY KEY,
                feature_type TEXT NOT NULL,
                question_or_request TEXT NOT NULL,
                expected_source TEXT,
                expected_intent TEXT,
                expected_operation TEXT,
                expected_file_ids TEXT,
                expected_chunk_ids TEXT,
                expected_evidence_ids TEXT,
                expected_answer_shape TEXT,
                expected_relation_type TEXT,
                expected_collection_members TEXT,
                gold_reason TEXT,
                split TEXT NOT NULL,
                dataset_version TEXT NOT NULL,
                metadata_json TEXT,
                created_at TEXT NOT NULL
              );
              CREATE INDEX idx_evaluation_cases_split ON evaluation_cases(split, feature_type);
              CREATE TABLE IF NOT EXISTS evaluation_runs (
                run_id TEXT PRIMARY KEY,
                dataset_version TEXT NOT NULL,
                code_revision TEXT,
                preset_id TEXT,
                model_ids TEXT,
                optimization_round INTEGER NOT NULL,
                started_at TEXT NOT NULL,
                completed_at TEXT,
                metrics_json TEXT
              );
              CREATE TABLE IF NOT EXISTS evaluation_results (
                result_id TEXT PRIMARY KEY,
                case_id TEXT NOT NULL,
                run_id TEXT NOT NULL,
                operation_id TEXT,
                pass_fail INTEGER NOT NULL,
                error_category TEXT,
                diagnosis_reason TEXT,
                actual_source TEXT,
                actual_intent TEXT,
                actual_operation TEXT,
                actual_files TEXT,
                actual_evidence TEXT,
                metrics_json TEXT,
                latency_ms INTEGER,
                created_at TEXT NOT NULL
              );
              CREATE INDEX idx_evaluation_results_run ON evaluation_results(run_id);
              CREATE INDEX idx_evaluation_results_case ON evaluation_results(case_id);",
    },
    Migration {
        version: 34,
        name: "image_ocr_before_vision",
        // 图片主链显式化：所有新提取图片先进入 OCR 队列；只有 OCR 失败、
        // 无文本、缺失/低置信度或复杂视觉才进入 VLM。OCR 与 VLM 分别保存
        // 尝试次数和幂等键，避免一个阶段的失败污染另一个阶段的检查点。
        sql: "ALTER TABLE image_assets ADD COLUMN ocr_confidence REAL;
              ALTER TABLE image_assets ADD COLUMN ocr_engine TEXT;
              ALTER TABLE image_assets ADD COLUMN vision_route_reason TEXT;
              ALTER TABLE image_assets ADD COLUMN ocr_attempt_count INTEGER NOT NULL DEFAULT 0;
              ALTER TABLE image_assets ADD COLUMN ocr_idempotency_key TEXT;
              ALTER TABLE image_assets ADD COLUMN ocr_error_json TEXT;
              ALTER TABLE ocr_attempts ADD COLUMN image_asset_id TEXT REFERENCES image_assets(asset_id) ON DELETE CASCADE;
              UPDATE image_assets
                 SET status = 'ready', vision_route_reason = 'ocr_success'
               WHERE status = 'pending_understanding'
                 AND description IS NULL
                 AND LENGTH(TRIM(COALESCE(ocr_text, ''))) > 0;
              UPDATE image_assets
                 SET status = 'pending_ocr', vision_route_reason = NULL
               WHERE status = 'pending_understanding'
                 AND description IS NULL
                 AND LENGTH(TRIM(COALESCE(ocr_text, ''))) = 0;
              CREATE INDEX idx_image_assets_ocr_queue
                ON image_assets(status, updated_at, asset_id)
                WHERE status IN ('pending_ocr', 'ocr_processing');",
    },
];

#[derive(Debug, Clone)]
pub struct CatalogStore {
    database_path: PathBuf,
    write_coordinator: Arc<WriteCoordinator>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WritePriority {
    Interactive,
    Background,
}

#[derive(Debug, Default)]
struct WriteCoordinatorState {
    active: bool,
    interactive_waiters: usize,
}

#[derive(Debug, Default)]
struct WriteCoordinator {
    state: Mutex<WriteCoordinatorState>,
    changed: Condvar,
}

struct WritePermit<'a> {
    coordinator: &'a WriteCoordinator,
}

impl WriteCoordinator {
    fn acquire(&self, priority: WritePriority) -> WritePermit<'_> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if priority == WritePriority::Interactive {
            state.interactive_waiters = state.interactive_waiters.saturating_add(1);
        }
        while state.active
            || (priority == WritePriority::Background && state.interactive_waiters > 0)
        {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(|error| error.into_inner());
        }
        if priority == WritePriority::Interactive {
            state.interactive_waiters = state.interactive_waiters.saturating_sub(1);
        }
        state.active = true;
        WritePermit { coordinator: self }
    }
}

impl Drop for WritePermit<'_> {
    fn drop(&mut self) {
        let mut state = self
            .coordinator
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.active = false;
        self.coordinator.changed.notify_all();
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct FileKeysetCursor {
    version: u8,
    filter_digest: String,
    last_seen_at: String,
    file_id: String,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct InboxKeysetCursor {
    version: u8,
    filter_digest: String,
    observed_at: String,
    inbox_id: String,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct AskSessionKeysetCursor {
    version: u8,
    updated_at: String,
    session_id: String,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct AskMessageKeysetCursor {
    version: u8,
    created_at: String,
    message_id: String,
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

/// 相邻块上下文注入上限：每个邻居块截取到该 token 数（估算权重口径），
/// 防止多证据 × 双邻居把小上下文窗口撑爆。
const NEIGHBOR_CONTEXT_TOKEN_CAP: u64 = 128;

/// 取命中块在同一节点内的前/后相邻块文本（ordinal ± 1），按 token 上限截断。
/// 邻居块本身已含 64 token 的重叠，此处再取全文纯粹是为生成模型补足
/// 「这段文字在原文中前后是什么」的线性语境。
fn fetch_neighbor_context(
    connection: &Connection,
    node_id: &str,
    ordinal: i64,
) -> Result<(Option<String>, Option<String>), AppError> {
    let mut statement = connection
        .prepare(
            "SELECT ordinal, text FROM chunks
             WHERE node_id = ?1 AND ordinal BETWEEN ?2 AND ?3
             ORDER BY ordinal",
        )
        .map_err(|error| storage_error("ASK_NEIGHBOR_QUERY_FAILED", error, true))?;
    let rows = statement
        .query_map(
            params![
                node_id,
                ordinal.saturating_sub(1),
                ordinal.saturating_add(1)
            ],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .map_err(|error| storage_error("ASK_NEIGHBOR_QUERY_FAILED", error, true))?;
    let mut before = None;
    let mut after = None;
    for row in rows {
        let (neighbor_ordinal, text) =
            row.map_err(|error| storage_error("ASK_NEIGHBOR_QUERY_FAILED", error, true))?;
        let capped = crate::indexing::cap_by_estimated_tokens(&text, NEIGHBOR_CONTEXT_TOKEN_CAP);
        if neighbor_ordinal < ordinal {
            before = Some(capped);
        } else if neighbor_ordinal > ordinal {
            after = Some(capped);
        }
    }
    Ok((before, after))
}

impl CatalogStore {
    pub fn open(database_path: impl Into<PathBuf>) -> Result<Self, AppError> {
        let store = Self {
            database_path: database_path.into(),
            write_coordinator: Arc::new(WriteCoordinator::default()),
        };
        if let Some(parent) = store.database_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| AppError::local_config(error.to_string(), true))?;
        }
        let mut connection = store.connect()?;
        connection
            .execute_batch("PRAGMA journal_mode = WAL;")
            .map_err(|error| storage_error("DATABASE_CONFIG_FAILED", error, true))?;
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
            .busy_timeout(Duration::from_millis(750))
            .map_err(|error| storage_error("DATABASE_CONFIG_FAILED", error, true))?;
        connection
            .execute_batch(
                "PRAGMA foreign_keys = ON; PRAGMA synchronous = NORMAL; PRAGMA temp_store = MEMORY;",
            )
            .map_err(|error| storage_error("DATABASE_CONFIG_FAILED", error, true))?;
        Ok(connection)
    }

    fn acquire_write(&self, priority: WritePriority) -> WritePermit<'_> {
        self.write_coordinator.acquire(priority)
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

    /// 读取用户当前选定的官方模型预设（preset_id），未选择时返回 `None`。
    /// 只保存 preset_id，不保存 UI 展示名，便于未来升级 Qwen/Embedding 而不破坏迁移。
    pub fn selected_preset_id(&self) -> Result<Option<String>, AppError> {
        let connection = self.connect()?;
        let value = connection
            .query_row(
                "SELECT value_json FROM application_settings WHERE setting_key = 'selected_preset_id'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| storage_error("SETTINGS_READ_FAILED", error, true))?;
        value
            .map(|raw| {
                serde_json::from_str::<String>(&raw)
                    .map_err(|error| AppError::new("SETTINGS_INVALID", error.to_string(), false))
            })
            .transpose()
    }

    /// 持久化当前选定的官方模型预设。旧配置（逐角色选择）迁移由上层完成，
    /// 这里只负责安全写入单个 preset_id。
    pub fn set_selected_preset_id(&self, preset_id: &str) -> Result<(), AppError> {
        let connection = self.connect()?;
        connection
            .execute(
                "INSERT INTO application_settings (setting_key, value_json, updated_at) VALUES ('selected_preset_id', ?1, ?2) ON CONFLICT(setting_key) DO UPDATE SET value_json = excluded.value_json, updated_at = excluded.updated_at",
                params![serde_json::to_string(preset_id).expect("str serializes"), Utc::now().to_rfc3339()],
            )
            .map_err(|error| storage_error("SETTINGS_WRITE_FAILED", error, true))?;
        Ok(())
    }

    /// 读取官方模型预设 schema 版本，未写入时返回 0（视为旧/未迁移状态）。
    pub fn model_preset_version(&self) -> Result<u32, AppError> {
        let connection = self.connect()?;
        let value = connection
            .query_row(
                "SELECT value_json FROM application_settings WHERE setting_key = 'model_preset_version'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| storage_error("SETTINGS_READ_FAILED", error, true))?;
        value
            .map(|raw| {
                serde_json::from_str::<u32>(&raw)
                    .map_err(|error| AppError::new("SETTINGS_INVALID", error.to_string(), false))
            })
            .transpose()
            .map(|version| version.unwrap_or(0))
    }

    /// 持久化官方模型预设 schema 版本。
    pub fn set_model_preset_version(&self, version: u32) -> Result<(), AppError> {
        let connection = self.connect()?;
        connection
            .execute(
                "INSERT INTO application_settings (setting_key, value_json, updated_at) VALUES ('model_preset_version', ?1, ?2) ON CONFLICT(setting_key) DO UPDATE SET value_json = excluded.value_json, updated_at = excluded.updated_at",
                params![serde_json::to_string(&version).expect("u32 serializes"), Utc::now().to_rfc3339()],
            )
            .map_err(|error| storage_error("SETTINGS_WRITE_FAILED", error, true))?;
        Ok(())
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
        let _permit = self.acquire_write(WritePriority::Background);
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
        let _permit = self.acquire_write(WritePriority::Interactive);
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
        let _permit = self.acquire_write(WritePriority::Interactive);
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
        let _permit = self.acquire_write(WritePriority::Interactive);
        let connection = self.connect()?;
        let changed = connection
            .execute(
                "UPDATE roots SET enabled = 0, user_disabled = 1, status = 'removing' WHERE root_id = ?1 AND enabled = 1",
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
        Ok(())
    }

    pub fn cleanup_disabled_root(&self, root_id: &Uuid) -> Result<u64, AppError> {
        const BATCH_SIZE: usize = 250;
        let mut removed = 0_u64;
        loop {
            let batch = {
                let connection = self.connect()?;
                let mut statement = connection
                    .prepare(
                        "SELECT file_id FROM file_root_memberships WHERE root_id = ?1 LIMIT ?2",
                    )
                    .map_err(|error| storage_error("MEMBERSHIP_QUERY_FAILED", error, true))?;
                statement
                    .query_map(params![root_id.to_string(), BATCH_SIZE as u64], |row| {
                        row.get::<_, String>(0)
                    })
                    .map_err(|error| storage_error("MEMBERSHIP_QUERY_FAILED", error, true))?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|error| storage_error("MEMBERSHIP_QUERY_FAILED", error, true))?
            };
            if batch.is_empty() {
                break;
            }
            {
                let _permit = self.acquire_write(WritePriority::Background);
                let mut connection = self.connect()?;
                let transaction = connection
                    .transaction()
                    .map_err(|error| storage_error("DATABASE_TRANSACTION_FAILED", error, true))?;
                for file_id in &batch {
                    transaction
                        .execute(
                            "DELETE FROM file_root_memberships WHERE root_id = ?1 AND file_id = ?2",
                            params![root_id.to_string(), file_id],
                        )
                        .map_err(|error| storage_error("MEMBERSHIP_DELETE_FAILED", error, true))?;
                    transaction
                        .execute(
                            "DELETE FROM files WHERE file_id = ?1 AND NOT EXISTS (SELECT 1 FROM file_root_memberships WHERE file_id = ?1)",
                            [file_id],
                        )
                        .map_err(|error| storage_error("ROOT_INDEX_PURGE_FAILED", error, true))?;
                }
                transaction
                    .commit()
                    .map_err(|error| storage_error("DATABASE_COMMIT_FAILED", error, true))?;
            }
            removed = removed.saturating_add(batch.len() as u64);
            std::thread::yield_now();
        }
        {
            let _permit = self.acquire_write(WritePriority::Background);
            let connection = self.connect()?;
            connection
                .execute("UPDATE file_root_memberships SET is_primary = 0", [])
                .map_err(|error| storage_error("MEMBERSHIP_UPDATE_FAILED", error, true))?;
            connection
                .execute(
                    "UPDATE file_root_memberships SET is_primary = 1 WHERE rowid IN (SELECT MIN(rowid) FROM file_root_memberships GROUP BY file_id)",
                    [],
                )
                .map_err(|error| storage_error("MEMBERSHIP_UPDATE_FAILED", error, true))?;
            refresh_root_coverage(&connection)?;
            connection
                .execute(
                    "UPDATE roots SET status = 'paused', file_count = 0 WHERE root_id = ?1 AND enabled = 0",
                    [root_id.to_string()],
                )
                .map_err(|error| storage_error("ROOT_UPDATE_FAILED", error, true))?;
        }
        Ok(removed)
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
        const SCAN_WRITE_BATCH: usize = 50;
        const MAX_LOCK_RETRIES: u32 = 5;
        let mut isolated_error_count = 0_u64;
        let mut total_lock_retries = 0_u32;

        for (batch_index, batch) in outcome.files.chunks(SCAN_WRITE_BATCH).enumerate() {
            let mut retry_count = 0_u32;
            loop {
                let write_result = (|| -> Result<u64, AppError> {
                    let _permit = self.acquire_write(WritePriority::Background);
                    let mut connection = self.connect()?;
                    let root_enabled = connection
                        .query_row(
                            "SELECT enabled FROM roots WHERE root_id = ?1",
                            [root_id.to_string()],
                            |row| row.get::<_, i64>(0),
                        )
                        .optional()
                        .map_err(|error| storage_error("ROOT_QUERY_FAILED", error, true))?
                        .unwrap_or_default()
                        != 0;
                    if !root_enabled {
                        return Err(AppError::new(
                            "SCAN_ROOT_REVOKED",
                            "资料位置授权已撤销，扫描结果未继续写入",
                            false,
                        ));
                    }
                    let transaction = connection.transaction().map_err(|error| {
                        storage_error("DATABASE_TRANSACTION_FAILED", error, true)
                    })?;
                    let mut batch_isolated_errors = 0_u64;
                    for file in batch {
                        let already_committed = transaction
                            .query_row(
                                "SELECT EXISTS(SELECT 1 FROM scan_seen_memberships s JOIN files f ON f.file_id = s.file_id WHERE s.job_id = ?1 AND s.root_id = ?2 AND f.path_key = ?3)",
                                params![job_id.to_string(), root_id.to_string(), file.path_key],
                                |row| row.get::<_, i64>(0),
                            )
                            .map_err(|error| {
                                storage_error("SCAN_CHECKPOINT_QUERY_FAILED", error, true)
                            })?
                            != 0;
                        if already_committed {
                            continue;
                        }

                        transaction
                            .execute_batch("SAVEPOINT scan_file")
                            .map_err(|error| storage_error("SCAN_SAVEPOINT_FAILED", error, true))?;
                        match upsert_file(&transaction, root_id, file) {
                            Ok(file_id) => {
                                transaction
                                    .execute(
                                        "INSERT OR IGNORE INTO scan_seen_memberships (job_id, root_id, file_id) VALUES (?1, ?2, ?3)",
                                        params![job_id.to_string(), root_id.to_string(), file_id.to_string()],
                                    )
                                    .map_err(|error| storage_error("SCAN_CHECKPOINT_WRITE_FAILED", error, true))?;
                                transaction.execute_batch("RELEASE scan_file").map_err(
                                    |error| storage_error("SCAN_SAVEPOINT_FAILED", error, true),
                                )?;
                            }
                            Err(error) if is_transient_storage_error(&error) => {
                                let _ = transaction
                                    .execute_batch("ROLLBACK TO scan_file; RELEASE scan_file");
                                return Err(error);
                            }
                            Err(error) => {
                                transaction
                                    .execute_batch("ROLLBACK TO scan_file; RELEASE scan_file")
                                    .map_err(|rollback_error| {
                                        storage_error(
                                            "SCAN_SAVEPOINT_ROLLBACK_FAILED",
                                            rollback_error,
                                            true,
                                        )
                                    })?;
                                let error_json =
                                    serde_json::to_string(&error).map_err(|json_error| {
                                        AppError::new(
                                            "PROCESSING_ATTEMPT_SERIALIZE_FAILED",
                                            json_error.to_string(),
                                            false,
                                        )
                                    })?;
                                transaction
                                    .execute(
                                        "INSERT INTO processing_attempts (attempt_id, file_id, revision_id, operation, engine, model_version, status, attempt_no, elapsed_ms, retryable, error_json, created_at) VALUES (?1, NULL, NULL, 'scan_upsert', 'sqlite', NULL, 'failed', 1, 0, ?2, ?3, ?4)",
                                        params![Uuid::now_v7().to_string(), if error.retryable { 1_i64 } else { 0_i64 }, error_json, Utc::now().to_rfc3339()],
                                    )
                                    .map_err(|database_error| storage_error("PROCESSING_ATTEMPT_WRITE_FAILED", database_error, true))?;
                                batch_isolated_errors = batch_isolated_errors.saturating_add(1);
                            }
                        }
                    }
                    let committed_items = transaction
                        .query_row(
                            "SELECT COUNT(*) FROM scan_seen_memberships WHERE job_id = ?1 AND root_id = ?2",
                            params![job_id.to_string(), root_id.to_string()],
                            |row| row.get::<_, u64>(0),
                        )
                        .map_err(|error| {
                            storage_error("SCAN_CHECKPOINT_QUERY_FAILED", error, true)
                        })?;
                    let enumerated_items =
                        ((batch_index + 1) * SCAN_WRITE_BATCH).min(outcome.files.len()) as u64;
                    let updated_at = Utc::now().to_rfc3339();
                    let total_isolated = isolated_error_count.saturating_add(batch_isolated_errors);
                    transaction
                        .execute(
                            "INSERT INTO scan_checkpoints (job_id, root_id, batch_no, enumerated_items, committed_items, isolated_failures, retry_count, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) ON CONFLICT(job_id) DO UPDATE SET batch_no = excluded.batch_no, enumerated_items = excluded.enumerated_items, committed_items = excluded.committed_items, isolated_failures = excluded.isolated_failures, retry_count = excluded.retry_count, updated_at = excluded.updated_at",
                            params![job_id.to_string(), root_id.to_string(), batch_index as u32, enumerated_items, committed_items, total_isolated, total_lock_retries.saturating_add(retry_count), updated_at],
                        )
                        .map_err(|error| storage_error("SCAN_CHECKPOINT_WRITE_FAILED", error, true))?;
                    let checkpoint_json = serde_json::json!({
                        "batch_no": batch_index,
                        "enumerated_items": enumerated_items,
                        "committed_items": committed_items,
                        "isolated_failures": total_isolated,
                        "retry_count": total_lock_retries.saturating_add(retry_count),
                    })
                    .to_string();
                    transaction
                        .execute(
                            "UPDATE jobs SET stage = 'committing', processed_items = ?1, total_items = ?2, progress = CASE WHEN ?2 = 0 THEN 0.9 ELSE MIN(0.95, 0.7 + (CAST(?1 AS REAL) / CAST(?2 AS REAL)) * 0.25) END, checkpoint_json = ?3, last_heartbeat_at = ?4 WHERE job_id = ?5",
                            params![committed_items, outcome.files.len() as u64, checkpoint_json, updated_at, job_id.to_string()],
                        )
                        .map_err(|error| storage_error("JOB_UPDATE_FAILED", error, true))?;
                    transaction
                        .commit()
                        .map_err(|error| storage_error("DATABASE_COMMIT_FAILED", error, true))?;
                    Ok(batch_isolated_errors)
                })();

                match write_result {
                    Ok(batch_errors) => {
                        isolated_error_count = isolated_error_count.saturating_add(batch_errors);
                        total_lock_retries = total_lock_retries.saturating_add(retry_count);
                        break;
                    }
                    Err(error)
                        if is_transient_storage_error(&error) && retry_count < MAX_LOCK_RETRIES =>
                    {
                        retry_count = retry_count.saturating_add(1);
                        let delay_ms = 25_u64.saturating_mul(1_u64 << (retry_count - 1));
                        std::thread::sleep(Duration::from_millis(delay_ms));
                    }
                    Err(mut error) => {
                        let mut details = error
                            .details
                            .take()
                            .and_then(|details| details.as_object().cloned())
                            .unwrap_or_default();
                        details.insert("batch_no".into(), serde_json::json!(batch_index));
                        details.insert("retry_count".into(), serde_json::json!(retry_count));
                        error.details = Some(Box::new(serde_json::Value::Object(details)));
                        return Err(error);
                    }
                }
            }
            std::thread::yield_now();
        }
        let total_error_count = outcome.error_count.saturating_add(isolated_error_count);
        if total_error_count == 0 && !outcome.deferred_by_budget {
            loop {
                let _permit = self.acquire_write(WritePriority::Background);
                let mut connection = self.connect()?;
                let transaction = connection
                    .transaction()
                    .map_err(|error| storage_error("DATABASE_TRANSACTION_FAILED", error, true))?;
                let removed = reconcile_root_memberships_batch(
                    &transaction,
                    root_id,
                    job_id,
                    SCAN_WRITE_BATCH,
                )?;
                transaction
                    .commit()
                    .map_err(|error| storage_error("DATABASE_COMMIT_FAILED", error, true))?;
                drop(_permit);
                if removed == 0 {
                    break;
                }
                std::thread::yield_now();
            }
        }

        let status = if total_error_count > 0 || outcome.deferred_by_budget {
            JobStatus::Partial
        } else {
            JobStatus::Succeeded
        };
        let root_status = if total_error_count > 0 {
            RootStatus::PartialDenied
        } else {
            RootStatus::Ready
        };
        let finished_at = Utc::now();
        let _permit = self.acquire_write(WritePriority::Background);
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction()
            .map_err(|error| storage_error("DATABASE_TRANSACTION_FAILED", error, true))?;
        let file_count = transaction
            .query_row(
                "SELECT COUNT(*) FROM scan_seen_memberships WHERE job_id = ?1 AND root_id = ?2",
                params![job_id.to_string(), root_id.to_string()],
                |row| row.get::<_, u64>(0),
            )
            .map_err(|error| storage_error("SCAN_CHECKPOINT_QUERY_FAILED", error, true))?;
        transaction
            .execute(
                "UPDATE roots SET status = ?1, file_count = ?2, error_count = ?3, permission_error_count = ?3, last_scanned_at = ?4, last_scan_at = ?4 WHERE root_id = ?5",
                params![
                    root_status.as_str(),
                    file_count,
                    total_error_count,
                    finished_at.to_rfc3339(),
                    root_id.to_string(),
                ],
            )
            .map_err(|error| storage_error("ROOT_UPDATE_FAILED", error, true))?;
        transaction
            .execute(
                "UPDATE jobs SET status = ?1, stage = 'completed', progress = 1.0, processed_items = ?2, total_items = ?2, finished_at = ?3, last_heartbeat_at = ?3 WHERE job_id = ?4",
                params![
                    status.as_str(),
                    outcome.files.len() as u64,
                    finished_at.to_rfc3339(),
                    job_id.to_string(),
                ],
            )
            .map_err(|error| storage_error("JOB_UPDATE_FAILED", error, true))?;
        transaction
            .execute(
                "DELETE FROM scan_seen_memberships WHERE job_id = ?1 AND root_id = ?2",
                params![job_id.to_string(), root_id.to_string()],
            )
            .map_err(|error| storage_error("SCAN_CHECKPOINT_WRITE_FAILED", error, true))?;
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
        let _permit = self.acquire_write(WritePriority::Background);
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

    pub fn processing_coverage_snapshot(&self) -> Result<ProcessingCoverageSnapshot, AppError> {
        let connection = self.connect()?;
        let discovered_files = connection
            .query_row(
                &format!("SELECT COUNT(*) FROM files f WHERE {AUTHORIZED_FILE_SQL}"),
                [],
                |row| row.get::<_, u64>(0),
            )
            .map_err(|error| storage_error("PROCESSING_COVERAGE_QUERY_FAILED", error, true))?;
        let (parseable_files, parsed_files, failed_files, explicitly_excluded_files) = connection
            .query_row(
                &format!(
                    "SELECT
                        SUM(CASE WHEN f.processing_disposition IN ('parseable_content','image_ocr','read_only_text','archive_manifest') THEN 1 ELSE 0 END),
                        SUM(CASE WHEN f.parse_status = 'parsed' THEN 1 ELSE 0 END),
                        SUM(CASE WHEN f.parse_status IN ('failed','encrypted') THEN 1 ELSE 0 END),
                        SUM(CASE WHEN f.processing_disposition IN ('media_metadata','safe_metadata','capability_missing','unknown') THEN 1 ELSE 0 END)
                     FROM files f WHERE {AUTHORIZED_FILE_SQL}"
                ),
                [],
                |row| {
                    Ok((
                        row.get::<_, Option<u64>>(0)?.unwrap_or_default(),
                        row.get::<_, Option<u64>>(1)?.unwrap_or_default(),
                        row.get::<_, Option<u64>>(2)?.unwrap_or_default(),
                        row.get::<_, Option<u64>>(3)?.unwrap_or_default(),
                    ))
                },
            )
            .map_err(|error| storage_error("PROCESSING_COVERAGE_QUERY_FAILED", error, true))?;
        let fts_chunks = connection
            .query_row(
                &format!("SELECT COUNT(*) FROM chunks c JOIN files f ON f.file_id = c.file_id WHERE f.current_revision_id = c.revision_id AND f.availability = 'present' AND {AUTHORIZED_FILE_SQL}"),
                [],
                |row| row.get::<_, u64>(0),
            )
            .map_err(|error| storage_error("PROCESSING_COVERAGE_QUERY_FAILED", error, true))?;
        let embedding_chunks = connection
            .query_row(
                &format!("SELECT COUNT(*) FROM chunk_embeddings e JOIN chunks c ON c.chunk_id = e.chunk_id AND c.file_id = e.file_id AND c.revision_id = e.revision_id JOIN files f ON f.file_id = e.file_id WHERE f.current_revision_id = e.revision_id AND f.availability = 'present' AND {AUTHORIZED_FILE_SQL}"),
                [],
                |row| row.get::<_, u64>(0),
            )
            .map_err(|error| storage_error("PROCESSING_COVERAGE_QUERY_FAILED", error, true))?;
        let active_vector_keys = connection
            .query_row(
                &format!("SELECT COUNT(*) FROM vector_index_keys k JOIN index_generations g ON g.generation_id = k.generation_id AND g.status = 'active' JOIN chunks c ON c.chunk_id = k.chunk_id AND c.file_id = k.file_id AND c.revision_id = k.revision_id JOIN files f ON f.file_id = k.file_id WHERE f.current_revision_id = k.revision_id AND f.availability = 'present' AND {AUTHORIZED_FILE_SQL}"),
                [],
                |row| row.get::<_, u64>(0),
            )
            .map_err(|error| storage_error("PROCESSING_COVERAGE_QUERY_FAILED", error, true))?;
        let (pending_ocr_assets, pending_vision_assets) = connection
            .query_row(
                "SELECT
                    SUM(CASE WHEN ocr_text IS NULL AND status IN ('pending_understanding','failed') THEN 1 ELSE 0 END),
                    SUM(CASE WHEN description IS NULL AND status IN ('pending_understanding','failed') THEN 1 ELSE 0 END)
                 FROM image_assets",
                [],
                |row| {
                    Ok((
                        row.get::<_, Option<u64>>(0)?.unwrap_or_default(),
                        row.get::<_, Option<u64>>(1)?.unwrap_or_default(),
                    ))
                },
            )
            .map_err(|error| storage_error("PROCESSING_COVERAGE_QUERY_FAILED", error, true))?;
        let parse_coverage = ratio(parsed_files, parseable_files);
        let embedding_coverage = ratio(embedding_chunks, fts_chunks);
        let vector_coverage = ratio(active_vector_keys, fts_chunks);
        Ok(ProcessingCoverageSnapshot {
            discovered_files,
            parseable_files,
            parsed_files,
            failed_files,
            explicitly_excluded_files,
            fts_chunks,
            embedding_chunks,
            active_vector_keys,
            pending_ocr_assets,
            pending_vision_assets,
            parse_coverage,
            embedding_coverage,
            vector_coverage,
            measured_at: Utc::now(),
        })
    }

    pub fn evaluation_integrity_snapshot(&self) -> Result<EvaluationIntegritySnapshot, AppError> {
        let connection = self.connect()?;
        let mut active_jobs = 0_u64;
        let mut stale_nonrecoverable_jobs = 0_u64;
        {
            let mut statement = connection
                .prepare(
                    "SELECT status, last_heartbeat_at, resume_count FROM jobs WHERE status IN ('queued','running','paused','awaiting_user')",
                )
                .map_err(|error| storage_error("EVALUATION_INTEGRITY_QUERY_FAILED", error, true))?;
            let rows = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, u64>(2)?,
                    ))
                })
                .map_err(|error| storage_error("EVALUATION_INTEGRITY_QUERY_FAILED", error, true))?;
            for row in rows {
                let (status, heartbeat, resume_count) = row.map_err(|error| {
                    storage_error("EVALUATION_INTEGRITY_QUERY_FAILED", error, true)
                })?;
                active_jobs += 1;
                let stale = status == "running"
                    && heartbeat
                        .as_deref()
                        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
                        .is_none_or(|value| {
                            Utc::now().signed_duration_since(value.with_timezone(&Utc))
                                > chrono::Duration::minutes(10)
                        });
                if stale && resume_count >= 3 {
                    stale_nonrecoverable_jobs += 1;
                }
            }
        }
        let orphan_embeddings = connection
            .query_row(
                "SELECT COUNT(*) FROM chunk_embeddings e LEFT JOIN chunks c ON c.chunk_id = e.chunk_id LEFT JOIN files f ON f.file_id = e.file_id WHERE c.chunk_id IS NULL OR f.file_id IS NULL",
                [],
                |row| row.get::<_, u64>(0),
            )
            .map_err(|error| storage_error("EVALUATION_INTEGRITY_QUERY_FAILED", error, true))?;
        let missing_vector_embeddings = connection
            .query_row(
                "SELECT COUNT(*) FROM vector_index_keys k JOIN index_generations g ON g.generation_id = k.generation_id AND g.status = 'active' LEFT JOIN chunk_embeddings e ON e.chunk_id = k.chunk_id AND e.model_artifact_id = g.model_artifact_id AND e.file_id = k.file_id AND e.revision_id = k.revision_id WHERE e.chunk_id IS NULL",
                [],
                |row| row.get::<_, u64>(0),
            )
            .map_err(|error| storage_error("EVALUATION_INTEGRITY_QUERY_FAILED", error, true))?;
        let generation_count_mismatches = connection
            .query_row(
                "SELECT COUNT(*) FROM index_generations g WHERE g.status = 'active' AND g.item_count != (SELECT COUNT(*) FROM vector_index_keys k WHERE k.generation_id = g.generation_id)",
                [],
                |row| row.get::<_, u64>(0),
            )
            .map_err(|error| storage_error("EVALUATION_INTEGRITY_QUERY_FAILED", error, true))?;
        let mut missing_active_index_files = 0_u64;
        {
            let mut statement = connection
                .prepare("SELECT index_path FROM index_generations WHERE status = 'active'")
                .map_err(|error| storage_error("EVALUATION_INTEGRITY_QUERY_FAILED", error, true))?;
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(|error| storage_error("EVALUATION_INTEGRITY_QUERY_FAILED", error, true))?;
            for row in rows {
                let path = row.map_err(|error| {
                    storage_error("EVALUATION_INTEGRITY_QUERY_FAILED", error, true)
                })?;
                if !PathBuf::from(path).is_file() {
                    missing_active_index_files += 1;
                }
            }
        }
        let files_without_authorized_roots = connection
            .query_row(
                "SELECT COUNT(*) FROM files f WHERE f.availability = 'present' AND NOT EXISTS (SELECT 1 FROM file_root_memberships m JOIN roots r ON r.root_id = m.root_id WHERE m.file_id = f.file_id AND r.enabled = 1)",
                [],
                |row| row.get::<_, u64>(0),
            )
            .map_err(|error| storage_error("EVALUATION_INTEGRITY_QUERY_FAILED", error, true))?;
        Ok(EvaluationIntegritySnapshot {
            active_jobs,
            stale_nonrecoverable_jobs,
            orphan_embeddings,
            inconsistent_active_vector_keys: missing_vector_embeddings
                .saturating_add(generation_count_mismatches),
            missing_active_index_files,
            files_without_authorized_roots,
            measured_at: Utc::now(),
        })
    }

    pub fn query_files(&self, request: &FileQuery) -> Result<FilePage, AppError> {
        request.validate_filters()?;
        let connection = self.connect()?;
        let page_size = usize::try_from(request.validated_page_size()).unwrap_or(200);
        let filter_digest = file_filter_digest(request);
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
        if let Some(encoded) = request.cursor.as_deref() {
            let cursor: FileKeysetCursor =
                decode_keyset_cursor(encoded, "FILE_CURSOR_INVALID", "资料分页游标无效或已过期")?;
            if cursor.version != 1 || cursor.filter_digest != filter_digest {
                return Err(AppError::new(
                    "FILE_CURSOR_INVALID",
                    "资料筛选条件已变化，请从第一页重新加载",
                    false,
                ));
            }
            predicates
                .push("(f.last_seen_at < ? OR (f.last_seen_at = ? AND f.file_id < ?))".into());
            values.push(SqlValue::Text(cursor.last_seen_at.clone()));
            values.push(SqlValue::Text(cursor.last_seen_at));
            values.push(SqlValue::Text(cursor.file_id));
        }
        let predicate = predicates.join(" AND ");
        let sql = format!(
            "{FILE_SELECT_WITH_ALIAS} WHERE {predicate} ORDER BY f.last_seen_at DESC, f.file_id DESC LIMIT ?"
        );
        let mut page_values = values;
        page_values.push(SqlValue::Integer((page_size + 1) as i64));
        let mut statement = connection
            .prepare(&sql)
            .map_err(|error| storage_error("FILE_QUERY_FAILED", error, true))?;
        let mut items = statement
            .query_map(params_from_iter(page_values.iter()), file_from_row)
            .map_err(|error| storage_error("FILE_QUERY_FAILED", error, true))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| storage_error("FILE_QUERY_FAILED", error, true))?;
        let has_more = items.len() > page_size;
        if has_more {
            items.truncate(page_size);
        }
        let next_cursor = if has_more {
            items
                .last()
                .map(|file| FileKeysetCursor {
                    version: 1,
                    filter_digest,
                    last_seen_at: file.last_seen_at.to_rfc3339(),
                    file_id: file.file_id.to_string(),
                })
                .map(encode_keyset_cursor)
                .transpose()?
        } else {
            None
        };
        Ok(FilePage {
            items,
            next_cursor,
            has_more,
            total: None,
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

    pub fn query_inbox(&self, query: &InboxQuery) -> Result<InboxPage, AppError> {
        query.validate()?;
        let connection = self.connect()?;
        let collections = collection_rows(&connection)?;
        let filter_digest = inbox_filter_digest(query);
        let mut predicates = vec![AUTHORIZED_FILE_SQL.to_owned()];
        let mut values = Vec::<SqlValue>::new();
        if query.status == TriageStatus::Error {
            predicates.push("i.resolution_status IN ('pending_retry','retrying')".into());
        } else if query.status != TriageStatus::All {
            predicates.push("i.triage_status = ?".into());
            values.push(SqlValue::Text(query.status.as_storage().into()));
        }
        if !query.event_types.is_empty() {
            predicates.push(format!(
                "i.event_type IN ({})",
                vec!["?"; query.event_types.len()].join(",")
            ));
            values.extend(
                query
                    .event_types
                    .iter()
                    .map(|value| SqlValue::Text(value.as_storage().into())),
            );
        }
        if !query.root_ids.is_empty() {
            predicates.push(format!(
                "EXISTS (SELECT 1 FROM file_root_memberships fm JOIN roots rr ON rr.root_id = fm.root_id WHERE fm.file_id = f.file_id AND rr.enabled = 1 AND fm.root_id IN ({}))",
                vec!["?"; query.root_ids.len()].join(",")
            ));
            values.extend(
                query
                    .root_ids
                    .iter()
                    .map(|value| SqlValue::Text(value.to_string())),
            );
        }
        if let Some(date_from) = query.date_from {
            predicates.push("i.observed_at >= ?".into());
            values.push(SqlValue::Text(date_from.to_rfc3339()));
        }
        if let Some(date_to) = query.date_to {
            predicates.push("i.observed_at <= ?".into());
            values.push(SqlValue::Text(date_to.to_rfc3339()));
        }
        if let Some(encoded) = query.cursor.as_deref() {
            let cursor: InboxKeysetCursor = decode_keyset_cursor(
                encoded,
                "INBOX_CURSOR_INVALID",
                "收件箱分页游标无效或已过期",
            )?;
            if cursor.version != 1 || cursor.filter_digest != filter_digest {
                return Err(AppError::new(
                    "INBOX_CURSOR_INVALID",
                    "收件箱筛选条件已变化，请从第一页重新加载",
                    false,
                ));
            }
            predicates.push("(i.observed_at < ? OR (i.observed_at = ? AND i.inbox_id < ?))".into());
            values.push(SqlValue::Text(cursor.observed_at.clone()));
            values.push(SqlValue::Text(cursor.observed_at));
            values.push(SqlValue::Text(cursor.inbox_id));
        }
        let sql = format!(
            "SELECT f.file_id, f.volume_id, f.canonical_path, f.display_name, f.extension, f.mime_type, f.size_bytes, f.fs_created_at, f.modified_at, f.windows_file_id, f.content_sha256, f.availability, f.current_revision_id, f.parse_status, f.first_seen_at, f.last_seen_at, i.inbox_id, i.event_type, i.observed_at, i.previous_path, i.triage_status, i.summary, i.error_code, i.resolution_status, i.attempt_count, i.last_attempt_at, (SELECT relation_id FROM file_relations r WHERE r.relation_type = 'exact_duplicate' AND (r.left_file_id = f.file_id OR r.right_file_id = f.file_id) AND r.review_status <> 'rejected' ORDER BY r.confidence DESC LIMIT 1) FROM inbox_events i JOIN files f ON f.file_id = i.file_id WHERE {} ORDER BY i.observed_at DESC, i.inbox_id DESC LIMIT ?",
            predicates.join(" AND ")
        );
        values.push(SqlValue::Integer(i64::from(query.page_size) + 1));
        let mut statement = connection
            .prepare(&sql)
            .map_err(|error| storage_error("INBOX_QUERY_FAILED", error, true))?;
        let rows = statement
            .query_map(params_from_iter(values.iter()), |row| {
                let file = file_from_row(row)?;
                let inbox_id: String = row.get(16)?;
                let event_type: String = row.get(17)?;
                let observed_at: String = row.get(18)?;
                let triage_status: String = row.get(20)?;
                let resolution_status: String = row.get(23)?;
                let last_attempt_at: Option<String> = row.get(25)?;
                let duplicate_group_id: Option<String> = row.get(26)?;
                Ok((
                    file,
                    parse_uuid_column(&inbox_id, 16)?,
                    InboxEventType::from_storage(&event_type),
                    parse_datetime_column(&observed_at, 18)?,
                    row.get::<_, Option<String>>(19)?,
                    TriageStatus::from_storage(&triage_status),
                    row.get::<_, Option<String>>(21)?,
                    row.get::<_, Option<String>>(22)?,
                    ResolutionStatus::from_storage(&resolution_status),
                    row.get::<_, u32>(24)?,
                    last_attempt_at
                        .map(|value| parse_datetime_column(&value, 25))
                        .transpose()?,
                    duplicate_group_id
                        .map(|value| parse_uuid_column(&value, 26))
                        .transpose()?,
                ))
            })
            .map_err(|error| storage_error("INBOX_QUERY_FAILED", error, true))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| storage_error("INBOX_QUERY_FAILED", error, true))?;
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
            resolution_status,
            attempt_count,
            last_attempt_at,
            duplicate_group_id,
        ) in rows
        {
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
                resolution_status,
                attempt_count,
                last_attempt_at,
                retry_action: retry_action_for(event_type, resolution_status),
                suggested_collection_ids,
                duplicate_group_id,
                summary,
                error_code,
            });
        }
        let has_more = items.len() > query.page_size as usize;
        let next_cursor = if has_more {
            items.truncate(query.page_size as usize);
            items
                .last()
                .map(|item| InboxKeysetCursor {
                    version: 1,
                    filter_digest,
                    observed_at: item.observed_at.to_rfc3339(),
                    inbox_id: item.inbox_id.to_string(),
                })
                .map(encode_keyset_cursor)
                .transpose()?
        } else {
            None
        };
        Ok(InboxPage {
            items,
            next_cursor,
            has_more,
        })
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
        let _permit = self.acquire_write(WritePriority::Interactive);
        let connection = self.connect()?;
        let changed = connection
            .execute(
                "UPDATE inbox_events SET triage_status = ?1, processed_at = CASE WHEN ?1 = 'new' THEN NULL ELSE ?2 END, resolution_status = CASE WHEN ?1 = 'ignored' AND resolution_status <> 'normal' THEN 'abandoned' WHEN ?1 = 'new' AND resolution_status = 'abandoned' THEN 'pending_retry' ELSE resolution_status END WHERE inbox_id = ?3",
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
        let collections = collection_rows(&connection)?;
        inbox_item_by_id(&connection, &request.inbox_id, &collections)?
            .ok_or_else(|| AppError::new("INBOX_ITEM_NOT_FOUND", "收件箱项目不可访问", false))
    }

    pub fn retry_inbox_item(&self, inbox_id: &Uuid) -> Result<InboxItem, AppError> {
        let _permit = self.acquire_write(WritePriority::Interactive);
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction()
            .map_err(|error| storage_error("INBOX_RETRY_FAILED", error, true))?;
        let row = transaction
            .query_row(
                &format!(
                    "SELECT i.file_id, i.event_type, i.resolution_status FROM inbox_events i JOIN files f ON f.file_id = i.file_id WHERE i.inbox_id = ?1 AND {AUTHORIZED_FILE_SQL} LIMIT 1"
                ),
                [inbox_id.to_string()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(|error| storage_error("INBOX_RETRY_FAILED", error, true))?
            .ok_or_else(|| {
                AppError::new("INBOX_ITEM_NOT_FOUND", "收件箱项目不存在或不可访问", false)
            })?;
        let (file_id, event_type, resolution_status) = row;
        if !matches!(event_type.as_str(), "parse_failed" | "ocr_required")
            || !matches!(resolution_status.as_str(), "pending_retry" | "retrying")
        {
            return Err(AppError::new(
                "INBOX_RETRY_NOT_AVAILABLE",
                "该项目当前没有可重试的处理任务",
                false,
            ));
        }
        let changed = transaction
            .execute(
                "UPDATE files SET parse_status = 'pending' WHERE file_id = ?1 AND availability = 'present' AND current_revision_id IS NOT NULL",
                [file_id],
            )
            .map_err(|error| storage_error("INBOX_RETRY_FAILED", error, true))?;
        if changed == 0 {
            return Err(AppError::new(
                "INBOX_RETRY_NOT_AVAILABLE",
                "资料已离线或当前修订不可用，无法重试",
                false,
            ));
        }
        transaction
            .execute(
                "UPDATE inbox_events SET triage_status = 'new', resolution_status = 'retrying', attempt_count = attempt_count + 1, last_attempt_at = ?1, processed_at = NULL WHERE inbox_id = ?2",
                params![Utc::now().to_rfc3339(), inbox_id.to_string()],
            )
            .map_err(|error| storage_error("INBOX_RETRY_FAILED", error, true))?;
        transaction
            .commit()
            .map_err(|error| storage_error("INBOX_RETRY_FAILED", error, true))?;
        let collections = collection_rows(&connection)?;
        inbox_item_by_id(&connection, inbox_id, &collections)?
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
        let _permit = self.acquire_write(WritePriority::Interactive);
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
        let _permit = self.acquire_write(WritePriority::Interactive);
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
        let _permit = self.acquire_write(WritePriority::Interactive);
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
        let _permit = self.acquire_write(WritePriority::Interactive);
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
        let _permit = self.acquire_write(WritePriority::Interactive);
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
        let offset = request
            .cursor
            .as_deref()
            .unwrap_or("0")
            .parse::<u64>()
            .map_err(|_| AppError::new("FILE_CURSOR_INVALID", "集合分页游标无效", false))?;
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
            has_more: consumed < total,
            total: Some(total),
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
        const ALGORITHM_VERSION: &str = "semantic_cluster_v3";
        /// 每次「AI分析新建议」最多展示的集合建议数；确认/拒绝后再次分析出下一批。
        const MAX_COLLECTION_SUGGESTIONS_PER_BATCH: usize = 5;
        let connection = self.connect()?;
        // 两个查询拆分（同 refresh_semantic_file_relations）：避免 3.50 bundled 病态计划的
        // 全量 JOIN（22.3s），先取 targets 元数据，再窗口取每文件前 3 个文本片段与向量。
        let targets_sql = format!(
            "WITH targets AS MATERIALIZED (SELECT f.file_id, f.current_revision_id, f.display_name FROM files f LEFT JOIN document_profiles p ON p.file_id = f.file_id WHERE f.current_revision_id IS NOT NULL AND f.parse_status = 'parsed' AND f.availability = 'present' AND {AUTHORIZED_FILE_SQL} AND EXISTS (SELECT 1 FROM chunk_embeddings ce WHERE ce.file_id = f.file_id AND ce.revision_id = f.current_revision_id AND ce.model_artifact_id = ?1) AND (p.file_id IS NULL OR p.revision_id <> f.current_revision_id OR p.embedding_model_id <> ?1 OR p.algorithm_version <> ?2) ORDER BY f.last_seen_at DESC LIMIT ?3) SELECT t.file_id, t.current_revision_id, t.display_name FROM targets t"
        );
        let targets = {
            let mut statement = connection
                .prepare(&targets_sql)
                .map_err(|error| storage_error("COLLECTION_PROFILE_QUERY_FAILED", error, true))?;
            statement
                .query_map(
                    params![model_artifact_id, ALGORITHM_VERSION, i64::from(max_files)],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                        ))
                    },
                )
                .map_err(|error| storage_error("COLLECTION_PROFILE_QUERY_FAILED", error, true))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| storage_error("COLLECTION_PROFILE_QUERY_FAILED", error, true))?
        };
        // 每文件前 3 片段（文本+向量）：窗口全量计算 + 外层过滤（无 CTE 无多表 JOIN）
        const TOP3_CHUNKS_SQL: &str = "SELECT file_id, text, dimension, vector_blob FROM (SELECT ce.file_id, c.text, ce.dimension, ce.vector_blob, ROW_NUMBER() OVER (PARTITION BY ce.file_id ORDER BY c.ordinal) AS rn FROM chunk_embeddings ce JOIN chunks c ON c.chunk_id = ce.chunk_id WHERE ce.model_artifact_id = ?1) WHERE rn <= 3";
        let mut chunks_by_file = HashMap::<String, Vec<(String, u32, Vec<u8>)>>::new();
        {
            let mut statement = connection
                .prepare(TOP3_CHUNKS_SQL)
                .map_err(|error| storage_error("COLLECTION_PROFILE_QUERY_FAILED", error, true))?;
            let rows = statement
                .query_map(params![model_artifact_id], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, u32>(2)?,
                        row.get::<_, Vec<u8>>(3)?,
                    ))
                })
                .map_err(|error| storage_error("COLLECTION_PROFILE_QUERY_FAILED", error, true))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| storage_error("COLLECTION_PROFILE_QUERY_FAILED", error, true))?;
            for (file_id, text, dimension, bytes) in rows {
                chunks_by_file
                    .entry(file_id)
                    .or_default()
                    .push((text, dimension, bytes));
            }
        }
        let mut grouped: BTreeMap<String, ProfileAggregate> = BTreeMap::new();
        for (file_id, revision_id, title) in targets {
            let Some(chunks) = chunks_by_file.get(&file_id) else {
                continue;
            };
            let mut texts = Vec::with_capacity(chunks.len());
            let mut vectors = Vec::with_capacity(chunks.len());
            for (text, dimension, bytes) in chunks {
                texts.push(text.clone());
                vectors.push(decode_vector(bytes, *dimension)?);
            }
            grouped.insert(file_id, (revision_id, title, texts, vectors));
        }
        let now = Utc::now().to_rfc3339();
        let mut profile_records = Vec::new();
        for (file_id, (revision_id, title, texts, vectors)) in grouped {
            let vector = mean_normalized_vector(&vectors)?;
            let bucket = semantic_bucket(&vector);
            let summary = texts
                .first()
                .map(|text| compact_profile_text(text, 260))
                .unwrap_or_default();
            let keywords = profile_keywords(&title);
            profile_records.push((
                file_id,
                revision_id,
                title,
                summary,
                serde_json::to_string(&keywords).unwrap_or_else(|_| "[]".into()),
                vector.len() as u32,
                encode_vector(&vector),
                bucket,
            ));
        }
        drop(connection);
        for batch in profile_records.chunks(100) {
            let _permit = self.acquire_write(WritePriority::Background);
            let mut connection = self.connect()?;
            let transaction = connection.transaction().map_err(|error| {
                storage_error("COLLECTION_SUGGESTION_REFRESH_FAILED", error, true)
            })?;
            for (file_id, revision_id, title, summary, keywords, dimension, vector, bucket) in batch
            {
                transaction
                    .execute(
                        "INSERT INTO document_profiles (file_id, revision_id, title, summary, keywords_json, entities_json, embedding_model_id, dimension, vector_blob, candidate_bucket, algorithm_version, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, '[]', ?6, ?7, ?8, ?9, ?10, ?11, ?11) ON CONFLICT(file_id) DO UPDATE SET revision_id = excluded.revision_id, title = excluded.title, summary = excluded.summary, keywords_json = excluded.keywords_json, entities_json = excluded.entities_json, embedding_model_id = excluded.embedding_model_id, dimension = excluded.dimension, vector_blob = excluded.vector_blob, candidate_bucket = excluded.candidate_bucket, algorithm_version = excluded.algorithm_version, updated_at = excluded.updated_at",
                        params![file_id, revision_id, title, summary, keywords, model_artifact_id, dimension, vector, bucket, ALGORITHM_VERSION, now],
                    )
                    .map_err(|error| storage_error("COLLECTION_PROFILE_WRITE_FAILED", error, true))?;
            }
            transaction.commit().map_err(|error| {
                storage_error("COLLECTION_SUGGESTION_REFRESH_FAILED", error, true)
            })?;
        }
        // 2. 种子扩展聚类：universe 加载全量档案（含本批刚档案化的），
        //    剔除已被任何集合建议消费过的文件，种子贪心扩展成互斥组。
        //    与旧实现（写 file_relations 边再连通分量）不同：集合建议不再
        //    读写 file_relations，与关系分析页彻底解耦。
        const COLLECTION_UNIVERSE_CAP: i64 = 4_000;
        let connection = self.connect()?;
        let mut consumed = HashSet::new();
        {
            let mut statement = connection
                .prepare("SELECT DISTINCT m.file_id FROM collection_suggested_members m")
                .map_err(|error| storage_error("COLLECTION_GROUP_QUERY_FAILED", error, true))?;
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(|error| storage_error("COLLECTION_GROUP_QUERY_FAILED", error, true))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| storage_error("COLLECTION_GROUP_QUERY_FAILED", error, true))?;
            for file_id in rows {
                consumed.insert(parse_uuid_value(&file_id)?);
            }
        }
        // 必须加载全量档案而非本批 targets：跨批次分组能力靠它保持，
        // 只取本批 targets 会让分到后批的文件永远组不成
        let universe_sql = format!(
            "SELECT p.file_id, p.revision_id, p.title, p.dimension, p.vector_blob, p.candidate_bucket FROM document_profiles p JOIN files f ON f.file_id = p.file_id WHERE p.embedding_model_id = ?1 AND p.algorithm_version = ?2 AND f.availability = 'present' AND {AUTHORIZED_FILE_SQL} ORDER BY p.updated_at DESC LIMIT ?3"
        );
        let mut seed_profiles = Vec::new();
        {
            let mut statement = connection
                .prepare(&universe_sql)
                .map_err(|error| storage_error("COLLECTION_UNIVERSE_QUERY_FAILED", error, true))?;
            let rows = statement
                .query_map(
                    params![
                        model_artifact_id,
                        ALGORITHM_VERSION,
                        COLLECTION_UNIVERSE_CAP
                    ],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, u32>(3)?,
                            row.get::<_, Vec<u8>>(4)?,
                            row.get::<_, String>(5)?,
                        ))
                    },
                )
                .map_err(|error| storage_error("COLLECTION_UNIVERSE_QUERY_FAILED", error, true))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| storage_error("COLLECTION_UNIVERSE_QUERY_FAILED", error, true))?;
            for (file_id, revision_id, title, dimension, bytes, bucket) in rows {
                seed_profiles.push(SeedProfile {
                    file_id: parse_uuid_value(&file_id)?,
                    revision_id: parse_uuid_value(&revision_id)?,
                    title,
                    vector: decode_vector(&bytes, dimension)?,
                    bucket,
                });
            }
        }
        drop(connection);
        let mut groups = seed_expand_semantic_groups(&seed_profiles, &consumed, 0.78, 12, 96);
        // 星形边数（种子→成员），仅 trace 用
        let candidate_edges = groups
            .iter()
            .map(|group| group.members.len())
            .sum::<usize>() as u64;

        // 3. 大组一致性校验：成员与组代表向量（均值）< 0.70 的踢出，
        //    防止「A~B、B~C、A~C 弱」的长链把不同主题连成一组。
        //    （种子扩展组由种子贪心而来，链式蔓延已不存在，此校验仅作兜底；
        //     种子不参与踢出——它是组的核，与成员相似度必 ≥0.78）
        {
            let mut kept = Vec::with_capacity(groups.len());
            for mut group in groups {
                if group.members.len() < 4 {
                    kept.push(group);
                    continue;
                }
                let Some(seed_vector) = seed_profiles
                    .iter()
                    .find(|profile| profile.file_id == group.seed_file_id)
                    .map(|profile| profile.vector.clone())
                else {
                    continue;
                };
                let mut all_vectors = Vec::with_capacity(group.members.len() + 1);
                all_vectors.push(seed_vector);
                let mut member_vectors = Vec::with_capacity(group.members.len());
                for member in &group.members {
                    match seed_profiles
                        .iter()
                        .find(|profile| profile.file_id == member.file_id)
                    {
                        Some(profile) => {
                            member_vectors.push((member.file_id, profile.vector.clone()));
                            all_vectors.push(profile.vector.clone());
                        }
                        None => {
                            member_vectors.clear();
                            break;
                        }
                    }
                }
                if member_vectors.is_empty() || all_vectors.is_empty() {
                    continue;
                }
                let representative = mean_normalized_vector(&all_vectors)?;
                let before = group.members.len();
                group.members.retain(|member| {
                    member_vectors
                        .iter()
                        .find(|(file_id, _)| *file_id == member.file_id)
                        .is_some_and(|(_, vector)| {
                            representative
                                .iter()
                                .zip(vector)
                                .map(|(a, b)| a * b)
                                .sum::<f32>()
                                >= 0.70
                        })
                });
                if group.members.len() >= 2 {
                    if group.members.len() < before {
                        group.confidence *=
                            f64::from(group.members.len() as u32) / f64::from(before as u32);
                    }
                    kept.push(group);
                }
            }
            groups = kept;
        }

        // 5. 每批最多展示 MAX_COLLECTION_SUGGESTIONS_PER_BATCH 组：跳过已被
        //    任何集合建议（含已确认/已拒绝）消费过的文件，按规模与置信度取前 N；
        //    确认或拒绝后再次分析即可看到下一批。
        let available = groups
            .into_iter()
            .filter(|group| {
                group
                    .members
                    .iter()
                    .all(|member| !consumed.contains(&member.file_id))
            })
            .collect::<Vec<_>>();
        let topic_groups = available.len() as u64;
        let mut ordered = available;
        ordered.sort_by(|left, right| {
            right.members.len().cmp(&left.members.len()).then(
                right
                    .confidence
                    .partial_cmp(&left.confidence)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
        });
        let shown = ordered
            .into_iter()
            .take(MAX_COLLECTION_SUGGESTIONS_PER_BATCH)
            .collect::<Vec<_>>();
        let remaining_topic_groups = topic_groups.saturating_sub(shown.len() as u64);

        let mut created_suggestions = 0_u64;
        let mut suggestion_ids = Vec::new();
        let mut seed_file_id_by_suggestion = HashMap::<Uuid, Uuid>::new();
        if !shown.is_empty() {
            let _permit = self.acquire_write(WritePriority::Background);
            let mut connection = self.connect()?;
            let transaction = connection.transaction().map_err(|error| {
                storage_error("COLLECTION_SUGGESTION_REFRESH_FAILED", error, true)
            })?;
            for group in shown {
                // 成员按 file_id 排序，revision/title 直接取聚类结果（universe 同源）
                let seed_title = seed_profiles
                    .iter()
                    .find(|profile| profile.file_id == group.seed_file_id)
                    .map(|profile| profile.title.as_str())
                    .unwrap_or("种子");
                let group_title = deterministic_collection_name(
                    &group
                        .members
                        .iter()
                        .map(|member| member.title.clone())
                        .collect::<Vec<_>>(),
                );
                let mut members = Vec::new();
                for member in &group.members {
                    members.push((
                        member.file_id,
                        member.revision_id,
                        member.title.clone(),
                        format!(
                            "与《{seed_title}》语义最相近（{:.0}%），组内平均置信度 {:.0}%",
                            member.similarity * 100.0,
                            group.confidence * 100.0
                        ),
                    ));
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
                let changed = transaction
                    .execute(
                        "INSERT OR IGNORE INTO collection_suggestions (suggestion_id, idempotency_key, suggested_name, description, confidence, status, model_version, algorithm_version, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, 'suggested', ?6, ?7, ?8, ?8)",
                        params![suggestion_id.to_string(), idempotency_key, group_title.clone(), "AI按同主题/同用途聚类生成的虚拟分类；不会改变任何原文件位置。", group.confidence, model_artifact_id, ALGORITHM_VERSION, now],
                    )
                    .map_err(|error| storage_error("COLLECTION_SUGGESTION_WRITE_FAILED", error, true))?;
                if changed == 0 {
                    continue;
                }
                for (file_id, revision_id, _, rationale) in &members {
                    transaction
                        .execute(
                            "INSERT INTO collection_suggested_members (suggestion_id, file_id, revision_id, confidence, rationale, state) VALUES (?1, ?2, ?3, ?4, ?5, 'suggested')",
                            params![suggestion_id.to_string(), file_id.to_string(), revision_id.to_string(), group.confidence, rationale],
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
                }
                created_suggestions += 1;
                suggestion_ids.push(suggestion_id);
                seed_file_id_by_suggestion.insert(suggestion_id, group.seed_file_id);
            }
            transaction.commit().map_err(|error| {
                storage_error("COLLECTION_SUGGESTION_REFRESH_FAILED", error, true)
            })?;
        }
        Ok(CollectionSuggestionRefreshResult {
            profiled_files: profile_records.len() as u64,
            candidate_edges,
            topic_groups,
            remaining_topic_groups,
            created_suggestions,
            suggestion_ids,
            seed_file_id_by_suggestion,
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

    /// 生成模型的命名润色：只改名称和说明，成员分组不动（完全由 Embedding 聚类决定）。
    pub fn apply_collection_model_naming(
        &self,
        suggestion_id: &Uuid,
        review: &CollectionModelReview,
        model_version: &str,
    ) -> Result<CollectionSuggestion, AppError> {
        if model_version.trim().is_empty()
            || review.suggested_name.trim().is_empty()
            || review.suggested_name.chars().count() > 40
            || review.description.chars().count() > 400
        {
            return Err(AppError::new(
                "COLLECTION_MODEL_REVIEW_INVALID",
                "AI集合命名的名称或说明无效",
                false,
            ));
        }
        let _permit = self.acquire_write(WritePriority::Interactive);
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction()
            .map_err(|error| storage_error("COLLECTION_MODEL_REVIEW_FAILED", error, true))?;
        ensure_suggestion_status(&transaction, suggestion_id, "suggested")?;
        let existing = query_suggestion_members(&transaction, suggestion_id)?;
        let member_titles = existing
            .iter()
            .map(|member| member.file.display_name.clone())
            .collect::<Vec<_>>();
        let proposed_name = review.suggested_name.trim();
        let repeats_member_name = member_titles.iter().any(|title| {
            collection_name_stem(title).eq_ignore_ascii_case(proposed_name)
                || title.eq_ignore_ascii_case(proposed_name)
        });
        let validated_name = if repeats_member_name || is_generic_collection_name(proposed_name) {
            deterministic_collection_name(&member_titles)
        } else {
            proposed_name.to_owned()
        };
        let now = Utc::now().to_rfc3339();
        transaction.execute("UPDATE collection_suggestions SET suggested_name = ?1, description = ?2, model_version = ?3, updated_at = ?4 WHERE suggestion_id = ?5", params![validated_name, review.description.trim(), model_version, now, suggestion_id.to_string()])
            .map_err(|error| storage_error("COLLECTION_MODEL_REVIEW_FAILED", error, true))?;
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
        let _permit = self.acquire_write(WritePriority::Interactive);
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
        let _permit = self.acquire_write(WritePriority::Interactive);
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
        let _permit = self.acquire_write(WritePriority::Interactive);
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

    /// 批量取建议成员的档案摘要（document_profiles.summary，260 字），file_id → summary。
    /// 供生成模型命名时参考每个成员的内容片段。
    pub fn collection_suggestion_member_summaries(
        &self,
        suggestion_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, String>, AppError> {
        if suggestion_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let placeholders = std::iter::repeat_n("?", suggestion_ids.len())
            .collect::<Vec<_>>()
            .join(",");
        let connection = self.connect()?;
        let sql = format!(
            "SELECT m.file_id, p.summary FROM collection_suggested_members m JOIN document_profiles p ON p.file_id = m.file_id WHERE m.suggestion_id IN ({placeholders})"
        );
        let mut statement = connection
            .prepare(&sql)
            .map_err(|error| storage_error("COLLECTION_SUMMARY_QUERY_FAILED", error, true))?;
        let rows = statement
            .query_map(
                params_from_iter(suggestion_ids.iter().map(|id| id.to_string())),
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .map_err(|error| storage_error("COLLECTION_SUMMARY_QUERY_FAILED", error, true))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| storage_error("COLLECTION_SUMMARY_QUERY_FAILED", error, true))?;
        let mut by_file = HashMap::with_capacity(rows.len());
        for (file_id, summary) in rows {
            by_file.insert(parse_uuid_value(&file_id)?, summary);
        }
        Ok(by_file)
    }

    /// rerank 修剪：删除被移除成员的行及对应 inbox 事件；剩余成员 <2 则整条建议作废
    /// （删 suggestion+成员+inbox）；否则按剩余成员（file_id 升序）重算并 UPDATE
    /// idempotency_key 与 confidence（剩余成员均值）——幂等键不重算的话，下次刷新
    /// 完整成员集会与旧键不匹配，INSERT OR IGNORE 不命中，产生重叠建议。
    /// 返回 true=存活，false=已作废。事务内完成。
    pub fn prune_collection_suggestion_members(
        &self,
        suggestion_id: &Uuid,
        removed_file_ids: &[Uuid],
    ) -> Result<bool, AppError> {
        if removed_file_ids.is_empty() {
            return Ok(true);
        }
        let _permit = self.acquire_write(WritePriority::Background);
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction()
            .map_err(|error| storage_error("COLLECTION_PRUNE_FAILED", error, true))?;
        let (algorithm_version, model_version): (String, String) = transaction
            .query_row(
                "SELECT algorithm_version, model_version FROM collection_suggestions WHERE suggestion_id = ?1",
                [suggestion_id.to_string()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .map_err(|error| storage_error("COLLECTION_PRUNE_FAILED", error, true))?;
        let placeholders = std::iter::repeat_n("?", removed_file_ids.len())
            .collect::<Vec<_>>()
            .join(",");
        transaction
            .execute(
                &format!(
                    "DELETE FROM collection_suggested_members WHERE suggestion_id = ?1 AND file_id IN ({placeholders})"
                ),
                params_from_iter(
                    std::iter::once(suggestion_id.to_string())
                        .chain(removed_file_ids.iter().map(|id| id.to_string())),
                ),
            )
            .map_err(|error| storage_error("COLLECTION_PRUNE_FAILED", error, true))?;
        // 只删被移除成员的 inbox 事件（存活成员的事件保留，等待审核）
        for removed in removed_file_ids {
            transaction
                .execute(
                    "DELETE FROM inbox_events WHERE dedupe_key = ?1",
                    [format!("collection_suggestion:{suggestion_id}:{removed}")],
                )
                .map_err(|error| storage_error("COLLECTION_PRUNE_FAILED", error, true))?;
        }
        // 剩余成员：按 file_id 排序重算幂等键与置信度（与写入侧口径一致）
        let remaining = {
            let mut statement = transaction
                .prepare(
                    "SELECT file_id, revision_id, confidence FROM collection_suggested_members WHERE suggestion_id = ?1 ORDER BY file_id",
                )
                .map_err(|error| storage_error("COLLECTION_PRUNE_FAILED", error, true))?;
            statement
                .query_map([suggestion_id.to_string()], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, f64>(2)?,
                    ))
                })
                .map_err(|error| storage_error("COLLECTION_PRUNE_FAILED", error, true))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| storage_error("COLLECTION_PRUNE_FAILED", error, true))?
        };
        if remaining.len() < 2 {
            // 存活成员不足两个，整条作废（成员行+建议行+全部 inbox 事件）
            transaction
                .execute(
                    "DELETE FROM collection_suggestions WHERE suggestion_id = ?1",
                    [suggestion_id.to_string()],
                )
                .map_err(|error| storage_error("COLLECTION_PRUNE_FAILED", error, true))?;
            transaction
                .execute(
                    "DELETE FROM collection_suggested_members WHERE suggestion_id = ?1",
                    [suggestion_id.to_string()],
                )
                .map_err(|error| storage_error("COLLECTION_PRUNE_FAILED", error, true))?;
            transaction
                .execute(
                    "DELETE FROM inbox_events WHERE dedupe_key LIKE ?1",
                    [format!("collection_suggestion:{suggestion_id}:%")],
                )
                .map_err(|error| storage_error("COLLECTION_PRUNE_FAILED", error, true))?;
            transaction
                .commit()
                .map_err(|error| storage_error("COLLECTION_PRUNE_FAILED", error, true))?;
            return Ok(false);
        }
        let mut digest = Sha256::new();
        digest.update(algorithm_version.as_bytes());
        digest.update(model_version.as_bytes());
        for (file_id, revision_id, _) in &remaining {
            digest.update(file_id.as_bytes());
            digest.update(revision_id.as_bytes());
        }
        let idempotency_key = format!("{:x}", digest.finalize());
        let confidence =
            remaining.iter().map(|(_, _, value)| *value).sum::<f64>() / remaining.len() as f64;
        let now = Utc::now().to_rfc3339();
        transaction
            .execute(
                "UPDATE collection_suggestions SET idempotency_key = ?1, confidence = ?2, updated_at = ?3 WHERE suggestion_id = ?4",
                params![idempotency_key, confidence, now, suggestion_id.to_string()],
            )
            .map_err(|error| storage_error("COLLECTION_PRUNE_FAILED", error, true))?;
        transaction
            .commit()
            .map_err(|error| storage_error("COLLECTION_PRUNE_FAILED", error, true))?;
        Ok(true)
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
        let mut hash_updates = Vec::<(Uuid, Option<Uuid>, String)>::new();
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
                hash_updates.push((file.file_id, file.current_revision_id, hash.clone()));
                hashed_files += 1;
                hash
            };
            hashes.entry(hash).or_default().push(file.file_id);
        }
        let _permit = self.acquire_write(WritePriority::Background);
        let transaction = connection
            .transaction()
            .map_err(|error| storage_error("DATABASE_TRANSACTION_FAILED", error, true))?;
        for (file_id, revision_id, hash) in hash_updates {
            transaction
                .execute(
                    "UPDATE files SET content_sha256 = ?1 WHERE file_id = ?2 AND current_revision_id = ?3",
                    params![hash, file_id.to_string(), revision_id.map(|value| value.to_string())],
                )
                .map_err(|error| storage_error("FILE_HASH_UPDATE_FAILED", error, true))?;
            if let Some(revision_id) = revision_id {
                transaction
                    .execute(
                        "UPDATE file_revisions SET content_sha256 = ?1 WHERE revision_id = ?2",
                        params![hash, revision_id.to_string()],
                    )
                    .map_err(|error| storage_error("FILE_HASH_UPDATE_FAILED", error, true))?;
            }
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
            semantic_related_pairs: 0,
            contains_or_summarizes_pairs: 0,
            groups_created: 0,
        })
    }

    pub fn list_file_relations(&self, limit: u32) -> Result<Vec<FileRelation>, AppError> {
        self.query_file_relations(&RelationQuery {
            cursor: None,
            page_size: limit,
            relation_type: None,
            review_status: None,
        })
        .map(|page| page.items)
    }

    /// 精确重复关系总数（首页指标）。首页只需计数，原实现拉 500 行再在内存里数，
    /// 每次轮询都全表读；过滤条件与 query_file_relations 的默认过滤保持一致
    ///（排除 rejected、只统计仍属于已启用根目录的文件）。
    pub fn count_exact_duplicate_relations(&self) -> Result<u64, AppError> {
        let connection = self.connect()?;
        connection
            .query_row(
                "SELECT COUNT(*) FROM file_relations r
                 WHERE relation_type = 'exact_duplicate'
                   AND review_status <> 'rejected'
                   AND EXISTS (SELECT 1 FROM file_root_memberships m JOIN roots rt ON rt.root_id = m.root_id WHERE m.file_id = r.left_file_id AND rt.enabled = 1)
                   AND EXISTS (SELECT 1 FROM file_root_memberships m JOIN roots rt ON rt.root_id = m.root_id WHERE m.file_id = r.right_file_id AND rt.enabled = 1)",
                [],
                |row| row.get::<_, u64>(0),
            )
            .map_err(|error| storage_error("RELATION_COUNT_FAILED", error, true))
    }

    pub fn refresh_semantic_file_relations(
        &self,
        model_artifact_id: &str,
        max_files: u32,
    ) -> Result<(u64, u64), AppError> {
        if model_artifact_id.trim().is_empty() || !(2..=20_000).contains(&max_files) {
            return Err(AppError::new(
                "SEMANTIC_RELATION_REFRESH_INVALID",
                "语义关系刷新需要有效的Embedding模型和文件预算",
                false,
            ));
        }
        let _permit = self.acquire_write(WritePriority::Background);
        const ALGORITHM_VERSION: &str = "semantic_relation_bucket_v1";
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction()
            .map_err(|error| storage_error("SEMANTIC_RELATION_REFRESH_FAILED", error, true))?;
        // 两个查询拆分：SQLite 3.50 bundled 对「物化 CTE JOIN 大表」的病态计划会让单查询
        // 全量读 197k 行（26.7s）。先取 targets 元数据，再窗口取每文件前 3 向量，内存侧关联。
        let targets_sql = format!(
            "WITH targets AS MATERIALIZED (SELECT f.file_id, f.current_revision_id, f.display_name, f.extension, f.size_bytes FROM files f WHERE f.current_revision_id IS NOT NULL AND f.parse_status = 'parsed' AND f.availability = 'present' AND {AUTHORIZED_FILE_SQL} AND EXISTS (SELECT 1 FROM chunk_embeddings ce WHERE ce.file_id = f.file_id AND ce.revision_id = f.current_revision_id AND ce.model_artifact_id = ?1) ORDER BY f.last_seen_at DESC LIMIT ?2) SELECT t.file_id, t.current_revision_id, t.display_name, t.extension, t.size_bytes FROM targets t"
        );
        let targets = {
            let mut statement = transaction
                .prepare(&targets_sql)
                .map_err(|error| storage_error("SEMANTIC_RELATION_QUERY_FAILED", error, true))?;
            statement
                .query_map(params![model_artifact_id, i64::from(max_files)], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, u64>(4)?,
                    ))
                })
                .map_err(|error| storage_error("SEMANTIC_RELATION_QUERY_FAILED", error, true))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| storage_error("SEMANTIC_RELATION_QUERY_FAILED", error, true))?
        };
        // 每文件前 3 向量：窗口函数全量计算 + 外层过滤（无 CTE 无多表 JOIN，3.50 下 6.7s）
        const TOP3_VECTORS_SQL: &str = "SELECT file_id, dimension, vector_blob FROM (SELECT ce.file_id, ce.dimension, ce.vector_blob, ROW_NUMBER() OVER (PARTITION BY ce.file_id ORDER BY c.ordinal) AS rn FROM chunk_embeddings ce JOIN chunks c ON c.chunk_id = ce.chunk_id WHERE ce.model_artifact_id = ?1) WHERE rn <= 3";
        let mut vectors_by_file = HashMap::<String, Vec<(u32, Vec<u8>)>>::new();
        {
            let mut statement = transaction
                .prepare(TOP3_VECTORS_SQL)
                .map_err(|error| storage_error("SEMANTIC_RELATION_QUERY_FAILED", error, true))?;
            let rows = statement
                .query_map(params![model_artifact_id], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, u32>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                    ))
                })
                .map_err(|error| storage_error("SEMANTIC_RELATION_QUERY_FAILED", error, true))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| storage_error("SEMANTIC_RELATION_QUERY_FAILED", error, true))?;
            for (file_id, dimension, bytes) in rows {
                vectors_by_file
                    .entry(file_id)
                    .or_default()
                    .push((dimension, bytes));
            }
        }
        type RelationProfileAggregate = (String, String, String, u64, Vec<Vec<f32>>);
        let mut grouped = BTreeMap::<String, RelationProfileAggregate>::new();
        for (file_id, revision_id, name, extension, size) in targets {
            let Some(vectors) = vectors_by_file.get(&file_id) else {
                continue;
            };
            let mut decoded = Vec::with_capacity(vectors.len());
            for (dimension, bytes) in vectors {
                decoded.push(decode_vector(bytes, *dimension)?);
            }
            grouped.insert(file_id, (revision_id, name, extension, size, decoded));
        }
        let mut profiles = Vec::new();
        for (file_id, (revision_id, name, extension, size, vectors)) in grouped {
            let vector = mean_normalized_vector(&vectors)?;
            profiles.push((
                parse_uuid_value(&file_id)?,
                parse_uuid_value(&revision_id)?,
                name,
                extension,
                size,
                semantic_bucket(&vector),
                vector,
            ));
        }
        transaction
            .execute(
                "DELETE FROM file_relations WHERE relation_type IN ('semantic_related','contains_or_summarizes') AND review_status = 'suggested' AND model_version = ?1",
                [model_artifact_id],
            )
            .map_err(|error| storage_error("SEMANTIC_RELATION_REFRESH_FAILED", error, true))?;
        let now = Utc::now().to_rfc3339();
        // 种子扩展（替代桶内两两全比较）：每个种子按相似度取 top-12 未消费邻居成组，
        // 组间互斥。星形边（seed→成员）保留「成员经 seed 连通」的分组能力；
        // 冗余两两边消失，边数变小是预期（evaluate_local 只记录不设门槛）。
        let mut seed_profiles = Vec::with_capacity(profiles.len());
        let mut profile_index = HashMap::<Uuid, usize>::new();
        for (index, profile) in profiles.iter().enumerate() {
            profile_index.insert(profile.0, index);
            seed_profiles.push(SeedProfile {
                file_id: profile.0,
                revision_id: profile.1,
                title: profile.2.clone(),
                vector: profile.6.clone(),
                bucket: profile.5.clone(),
            });
        }
        let mut semantic_pairs = 0_u64;
        let mut contains_pairs = 0_u64;
        let mut touched_files = HashSet::new();
        let groups = seed_expand_semantic_groups(&seed_profiles, &HashSet::new(), 0.78, 12, 96);
        for group in &groups {
            let Some(&seed_index) = profile_index.get(&group.seed_file_id) else {
                continue;
            };
            for member in &group.members {
                let Some(&member_index) = profile_index.get(&member.file_id) else {
                    continue;
                };
                let left_profile = &profiles[seed_index];
                let right_profile = &profiles[member_index];
                let similarity = member.similarity;
                let left_summary = is_summary_like_name(&left_profile.2);
                let right_summary = is_summary_like_name(&right_profile.2);
                let size_ratio = (left_profile.4.min(right_profile.4) as f64)
                    / (left_profile.4.max(right_profile.4).max(1) as f64);
                let contains =
                    similarity >= 0.84 && ((left_summary != right_summary) || size_ratio <= 0.42);
                let relation_type = if contains {
                    RelationType::ContainsOrSummarizes
                } else {
                    RelationType::SemanticRelated
                };
                let direction_reason = if contains {
                    let (summary, source) = if left_summary || left_profile.4 <= right_profile.4 {
                        (&left_profile.2, &right_profile.2)
                    } else {
                        (&right_profile.2, &left_profile.2)
                    };
                    format!("《{summary}》可能是《{source}》的摘要、提纲或派生资料")
                } else {
                    format!(
                        "两份资料的文档级语义相似度为 {:.0}%",
                        f64::from(similarity) * 100.0
                    )
                };
                let (left_id, right_id, left_revision, right_revision) =
                    if left_profile.0 < right_profile.0 {
                        (
                            left_profile.0,
                            right_profile.0,
                            left_profile.1,
                            right_profile.1,
                        )
                    } else {
                        (
                            right_profile.0,
                            left_profile.0,
                            right_profile.1,
                            left_profile.1,
                        )
                    };
                let reasons = vec![
                    direction_reason,
                    format!(
                        "共同内容候选桶 {}，文件类型 {} / {}",
                        left_profile.5, left_profile.3, right_profile.3
                    ),
                ];
                let reasons_json = serde_json::to_string(&reasons).map_err(|error| {
                    AppError::new("RELATION_DATA_INVALID", error.to_string(), false)
                })?;
                transaction.execute(
                    "INSERT INTO file_relations (relation_id, left_file_id, right_file_id, relation_type, confidence, reasons_json, review_status, created_at, updated_at, algorithm_version, model_version, left_revision_id, right_revision_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'suggested', ?7, ?7, ?8, ?9, ?10, ?11) ON CONFLICT(left_file_id, right_file_id, relation_type) DO UPDATE SET confidence = excluded.confidence, reasons_json = excluded.reasons_json, review_status = CASE WHEN file_relations.algorithm_version IS NOT excluded.algorithm_version OR file_relations.model_version IS NOT excluded.model_version OR file_relations.left_revision_id IS NOT excluded.left_revision_id OR file_relations.right_revision_id IS NOT excluded.right_revision_id THEN 'suggested' ELSE file_relations.review_status END, algorithm_version = excluded.algorithm_version, model_version = excluded.model_version, left_revision_id = excluded.left_revision_id, right_revision_id = excluded.right_revision_id, updated_at = excluded.updated_at",
                    params![Uuid::now_v7().to_string(), left_id.to_string(), right_id.to_string(), relation_type.as_storage(), f64::from(similarity), reasons_json, now, ALGORITHM_VERSION, model_artifact_id, left_revision.to_string(), right_revision.to_string()],
                ).map_err(|error| storage_error("SEMANTIC_RELATION_WRITE_FAILED", error, true))?;
                touched_files.insert(left_id);
                touched_files.insert(right_id);
                if contains {
                    contains_pairs += 1;
                } else {
                    semantic_pairs += 1;
                }
            }
        }
        for file_id in touched_files {
            let revision_key = profiles
                .iter()
                .find(|profile| profile.0 == file_id)
                .map(|profile| profile.1.to_string())
                .unwrap_or_else(|| "no-revision".into());
            insert_inbox_event(
                &transaction,
                &file_id,
                InboxEventType::RelationSuggested,
                Utc::now(),
                None,
                TriageStatus::New,
                Some("关系分析发现同主题、同用途或包含关系，等待人工复核"),
                None,
                &format!("semantic_relation:{model_artifact_id}:{file_id}:{revision_key}"),
            )?;
        }
        transaction
            .commit()
            .map_err(|error| storage_error("SEMANTIC_RELATION_REFRESH_FAILED", error, true))?;
        Ok((semantic_pairs, contains_pairs))
    }

    pub fn query_file_relations(&self, request: &RelationQuery) -> Result<RelationPage, AppError> {
        request.validate()?;
        let offset = request.offset()?;
        let page_size = u64::from(request.page_size);
        let connection = self.connect()?;
        let mut predicates = vec![
            "EXISTS (SELECT 1 FROM file_root_memberships m JOIN roots rt ON rt.root_id = m.root_id WHERE m.file_id = r.left_file_id AND rt.enabled = 1)".to_owned(),
            "EXISTS (SELECT 1 FROM file_root_memberships m JOIN roots rt ON rt.root_id = m.root_id WHERE m.file_id = r.right_file_id AND rt.enabled = 1)".to_owned(),
        ];
        if let Some(relation_type) = request.relation_type {
            predicates.push(format!("relation_type = '{}'", relation_type.as_storage()));
        }
        if let Some(review_status) = request.review_status.as_deref() {
            predicates.push(format!("review_status = '{review_status}'"));
        } else {
            predicates.push("review_status <> 'rejected'".to_owned());
        }
        let predicate = predicates.join(" AND ");
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
        let _permit = self.acquire_write(WritePriority::Interactive);
        let connection = self.connect()?;
        let changed = connection
            .execute(
                "UPDATE file_relations AS r SET review_status = ?1, updated_at = ?2 WHERE relation_id = ?3 AND EXISTS (SELECT 1 FROM file_root_memberships m JOIN roots rt ON rt.root_id = m.root_id WHERE m.file_id = r.left_file_id AND rt.enabled = 1) AND EXISTS (SELECT 1 FROM file_root_memberships m JOIN roots rt ON rt.root_id = m.root_id WHERE m.file_id = r.right_file_id AND rt.enabled = 1)",
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

    pub fn review_file_relations(
        &self,
        relation_ids: &[Uuid],
        action: &str,
    ) -> Result<u64, AppError> {
        if relation_ids.is_empty() || relation_ids.len() > 500 {
            return Err(AppError::new(
                "RELATION_REVIEW_INVALID",
                "每次批量复核需要选择1到500条文件关系",
                false,
            ));
        }
        if !matches!(action, "accepted" | "rejected") {
            return Err(AppError::new(
                "RELATION_REVIEW_INVALID",
                "关系复核动作只能是 accepted 或 rejected",
                false,
            ));
        }
        let _permit = self.acquire_write(WritePriority::Interactive);
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction()
            .map_err(|error| storage_error("RELATION_REVIEW_FAILED", error, true))?;
        let updated_at = Utc::now().to_rfc3339();
        let mut changed = 0_u64;
        for relation_id in relation_ids.iter().collect::<HashSet<_>>() {
            changed = changed.saturating_add(
                transaction
                    .execute(
                        "UPDATE file_relations AS r SET review_status = ?1, updated_at = ?2 WHERE relation_id = ?3 AND EXISTS (SELECT 1 FROM file_root_memberships m JOIN roots rt ON rt.root_id = m.root_id WHERE m.file_id = r.left_file_id AND rt.enabled = 1) AND EXISTS (SELECT 1 FROM file_root_memberships m JOIN roots rt ON rt.root_id = m.root_id WHERE m.file_id = r.right_file_id AND rt.enabled = 1)",
                        params![action, updated_at, relation_id.to_string()],
                    )
                    .map_err(|error| storage_error("RELATION_REVIEW_FAILED", error, true))?
                    as u64,
            );
        }
        if changed == 0 {
            return Err(AppError::new(
                "RELATION_REVIEW_INVALID",
                "所选文件关系不存在或已不在授权范围",
                false,
            ));
        }
        transaction
            .commit()
            .map_err(|error| storage_error("RELATION_REVIEW_FAILED", error, true))?;
        Ok(changed)
    }

    /// 把当前（非已排除的）文件关系边聚类成组并落库。
    ///
    /// 流程：读边 → version 候选边用真实内容相似度校准（<0.84 降级为语义相关）
    /// → 连通分量聚类 → 大语义组做组代表向量一致性校验（防长链蔓延）
    /// → 版本族角色判定（最新版 / 近重复副本）→ 删旧组、插新组。
    pub fn refresh_relation_groups(
        &self,
        model_artifact_id: Option<&str>,
    ) -> Result<u64, AppError> {
        let _permit = self.acquire_write(WritePriority::Background);
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction()
            .map_err(|error| storage_error("RELATION_GROUP_REFRESH_FAILED", error, true))?;
        let now = Utc::now().to_rfc3339();

        // 1. 读取非排除边与文件信息
        let mut edges = Vec::new();
        let files = list_files_with_connection(&transaction)?
            .into_iter()
            .filter(|file| {
                file.availability == crate::Availability::Present
                    && file_is_authorized(&transaction, &file.file_id).unwrap_or(false)
            })
            .collect::<Vec<_>>();
        let mut file_by_id = HashMap::<Uuid, &FileRecord>::new();
        for file in &files {
            file_by_id.insert(file.file_id, file);
        }
        let relation_sql = "SELECT relation_id, relation_type, left_file_id, right_file_id, confidence, review_status FROM file_relations WHERE review_status <> 'rejected'";
        let mut statement = transaction
            .prepare(relation_sql)
            .map_err(|error| storage_error("RELATION_GROUP_REFRESH_FAILED", error, true))?;
        let raw_edges = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, f64>(4)?,
                    row.get::<_, String>(5)?,
                ))
            })
            .map_err(|error| storage_error("RELATION_GROUP_REFRESH_FAILED", error, true))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| storage_error("RELATION_GROUP_REFRESH_FAILED", error, true))?;
        drop(statement);
        for (_relation_id, relation_type, left, right, confidence, _review_status) in raw_edges {
            let left_id = parse_uuid_value(&left)?;
            let right_id = parse_uuid_value(&right)?;
            if !file_by_id.contains_key(&left_id) || !file_by_id.contains_key(&right_id) {
                continue;
            }
            let relation_type = crate::RelationType::from_storage(&relation_type);
            edges.push(RelationEdge {
                left_file_id: left_id,
                right_file_id: right_id,
                relation_type,
                confidence,
            });
        }

        // 2. version 候选边用内容相似度校准（有 embedding 时）
        let mut vector_cache = HashMap::<Uuid, Vec<f32>>::new();
        let version_edges = edges
            .iter_mut()
            .filter(|edge| edge.relation_type == crate::RelationType::VersionCandidate)
            .collect::<Vec<_>>();
        for edge in version_edges {
            let Some(left_vector) = file_vector_for(
                &transaction,
                model_artifact_id,
                edge.left_file_id,
                &mut vector_cache,
            )?
            else {
                continue;
            };
            let Some(right_vector) = file_vector_for(
                &transaction,
                model_artifact_id,
                edge.right_file_id,
                &mut vector_cache,
            )?
            else {
                continue;
            };
            let similarity = left_vector
                .iter()
                .zip(&right_vector)
                .map(|(a, b)| a * b)
                .sum::<f32>();
            if similarity < 0.84 {
                // 同名但内容差异大：不是版本，降级为语义相关。
                // 若语义分析已建立同对 semantic_related 关系，先删 version_candidate 行
                // 避免 UNIQUE(left_file_id, right_file_id, relation_type) 冲突，保留语义置信度。
                // 注意：不用嵌套 EXISTS 子查询（SQLite 3.50 bundled 对子查询+主查询的病态计划
                // 会造成假阴性，导致 DELETE 漏删后 UPDATE 撞 UNIQUE），改为先查后写。
                let has_sr = transaction
                    .query_row(
                        "SELECT 1 FROM file_relations WHERE left_file_id = ?1 AND right_file_id = ?2 AND relation_type = 'semantic_related' LIMIT 1",
                        params![edge.left_file_id.to_string(), edge.right_file_id.to_string()],
                        |_| Ok(()),
                    )
                    .optional()
                    .map_err(|error| storage_error("RELATION_GROUP_REFRESH_FAILED", error, true))?;
                if has_sr.is_some() {
                    transaction
                        .execute(
                            "DELETE FROM file_relations WHERE left_file_id = ?1 AND right_file_id = ?2 AND relation_type = 'version_candidate'",
                            params![edge.left_file_id.to_string(), edge.right_file_id.to_string()],
                        )
                        .map_err(|error| storage_error("RELATION_GROUP_REFRESH_FAILED", error, true))?;
                } else {
                    transaction
                        .execute(
                            "UPDATE file_relations SET relation_type = 'semantic_related', confidence = ?1, updated_at = ?2 WHERE left_file_id = ?3 AND right_file_id = ?4 AND relation_type = 'version_candidate'",
                            params![f64::from(similarity), now, edge.left_file_id.to_string(), edge.right_file_id.to_string()],
                        )
                        .map_err(|error| storage_error("RELATION_GROUP_REFRESH_FAILED", error, true))?;
                }
                edge.relation_type = crate::RelationType::SemanticRelated;
            } else {
                transaction
                    .execute(
                        "UPDATE file_relations SET confidence = ?1, updated_at = ?2 WHERE left_file_id = ?3 AND right_file_id = ?4 AND relation_type = 'version_candidate'",
                        params![f64::from(similarity), now, edge.left_file_id.to_string(), edge.right_file_id.to_string()],
                    )
                    .map_err(|error| storage_error("RELATION_GROUP_REFRESH_FAILED", error, true))?;
            }
            edge.confidence = f64::from(similarity);
        }

        // 3. 连通分量聚类
        let names_by_id = files
            .iter()
            .map(|file| (file.file_id, file.display_name.clone()))
            .collect::<HashMap<_, _>>();
        let title_for = |member_ids: &[Uuid], group_type: crate::RelationGroupType| -> String {
            let names = member_ids
                .iter()
                .filter_map(|id| names_by_id.get(id).cloned())
                .collect::<Vec<_>>();
            match group_type {
                RelationGroupType::Duplicate => {
                    format!("{} 份完全重复文件", names.len())
                }
                RelationGroupType::VersionFamily => {
                    let stems = names
                        .iter()
                        .filter_map(|name| {
                            let key = normalized_version_key(name);
                            (!key.is_empty()).then_some(key)
                        })
                        .collect::<HashSet<_>>();
                    let stem = {
                        let mut sorted = stems.into_iter().collect::<Vec<_>>();
                        sorted.sort();
                        sorted.join(" / ")
                    };
                    if stem.is_empty() {
                        format!("版本族 · {} 个文件", names.len())
                    } else {
                        format!("{stem} · 版本族（{} 个文件）", names.len())
                    }
                }
                _ => deterministic_collection_name(&names),
            }
        };
        let mut groups = cluster_relation_edges(&edges, &title_for);

        // 4. 大语义组一致性校验：成员与组代表向量（均值）< 0.70 的踢出，
        //    防止「A~B、B~C、A~C 弱」的长链把不同主题连成一组。
        if model_artifact_id.is_some() {
            let mut kept = Vec::with_capacity(groups.len());
            for mut group in groups {
                if group.members.len() < 4
                    || !matches!(
                        group.group_type,
                        RelationGroupType::TopicGroup | RelationGroupType::Mixed
                    )
                {
                    kept.push(group);
                    continue;
                }
                let mut vectors = Vec::new();
                let mut valid = true;
                for member in &group.members {
                    match file_vector_for(
                        &transaction,
                        model_artifact_id,
                        member.file_id,
                        &mut vector_cache,
                    )? {
                        Some(vector) => vectors.push((member.file_id, vector)),
                        None => {
                            valid = false;
                            break;
                        }
                    }
                }
                if !valid || vectors.is_empty() {
                    continue;
                }
                let representative = mean_normalized_vector(
                    &vectors
                        .iter()
                        .map(|(_, vector)| vector.clone())
                        .collect::<Vec<_>>(),
                )?;
                let before = group.members.clone();
                group.members.retain(|member| {
                    vectors
                        .iter()
                        .find(|(file_id, _)| *file_id == member.file_id)
                        .is_some_and(|(_, vector)| {
                            representative
                                .iter()
                                .zip(vector)
                                .map(|(a, b)| a * b)
                                .sum::<f32>()
                                >= 0.70
                        })
                });
                if group.members.len() >= 2 {
                    if group.members.len() < before.len() {
                        group.confidence *=
                            f64::from(group.members.len() as u32) / f64::from(before.len() as u32);
                    }
                    kept.push(group);
                }
            }
            groups = kept;
        }

        // 5. 版本族角色判定：修改时间最新 → latest；与最新内容相似 ≥0.99 → copy
        for group in &mut groups {
            if group.group_type != RelationGroupType::VersionFamily {
                continue;
            }
            let latest_id = group
                .members
                .iter()
                .filter_map(|member| {
                    file_by_id
                        .get(&member.file_id)
                        .map(|file| (member.file_id, file.fs_modified_at))
                })
                .max_by(|left, right| left.1.cmp(&right.1))
                .map(|(file_id, _)| file_id);
            if let Some(latest_id) = latest_id {
                for member in &mut group.members {
                    if member.file_id == latest_id {
                        member.role = RelationGroupRole::Latest;
                    }
                }
                let Some(latest_vector) = file_vector_for(
                    &transaction,
                    model_artifact_id,
                    latest_id,
                    &mut vector_cache,
                )?
                else {
                    continue;
                };
                for member in &mut group.members {
                    if member.role == RelationGroupRole::Latest {
                        continue;
                    }
                    let Some(vector) = file_vector_for(
                        &transaction,
                        model_artifact_id,
                        member.file_id,
                        &mut vector_cache,
                    )?
                    else {
                        continue;
                    };
                    let similarity = latest_vector
                        .iter()
                        .zip(&vector)
                        .map(|(a, b)| a * b)
                        .sum::<f32>();
                    if similarity >= 0.99 {
                        member.role = RelationGroupRole::Copy;
                    }
                }
            }
        }

        // 6. 删旧 suggested 组，插新组
        transaction
            .execute(
                "DELETE FROM relation_group_members WHERE group_id IN (SELECT group_id FROM relation_groups WHERE review_status = 'suggested')",
                [],
            )
            .map_err(|error| storage_error("RELATION_GROUP_REFRESH_FAILED", error, true))?;
        transaction
            .execute(
                "DELETE FROM relation_groups WHERE review_status = 'suggested'",
                [],
            )
            .map_err(|error| storage_error("RELATION_GROUP_REFRESH_FAILED", error, true))?;
        for group in &groups {
            let group_id = Uuid::now_v7();
            transaction
                .execute(
                    "INSERT INTO relation_groups (group_id, group_type, title, confidence, member_count, review_status, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, 'suggested', ?6, ?6)",
                    params![group_id.to_string(), group.group_type.as_storage(), group.title, group.confidence, group.members.len() as u32, now],
                )
                .map_err(|error| storage_error("RELATION_GROUP_REFRESH_FAILED", error, true))?;
            for member in &group.members {
                transaction
                    .execute(
                        "INSERT INTO relation_group_members (group_id, file_id, role) VALUES (?1, ?2, ?3)",
                        params![group_id.to_string(), member.file_id.to_string(), member.role.as_storage()],
                    )
                    .map_err(|error| storage_error("RELATION_GROUP_REFRESH_FAILED", error, true))?;
            }
        }
        transaction
            .commit()
            .map_err(|error| storage_error("RELATION_GROUP_REFRESH_FAILED", error, true))?;
        Ok(groups.len() as u64)
    }

    pub fn query_relation_groups(
        &self,
        request: &RelationGroupQuery,
    ) -> Result<RelationGroupPage, AppError> {
        request.validate()?;
        let offset = request.offset()?;
        let page_size = u64::from(request.page_size);
        let connection = self.connect()?;
        let mut predicates = Vec::<String>::new();
        if let Some(group_type) = request.group_type {
            predicates.push(format!("group_type = '{}'", group_type.as_storage()));
        }
        if let Some(review_status) = request.review_status.as_deref() {
            predicates.push(format!("review_status = '{review_status}'"));
        } else {
            predicates.push("review_status <> 'rejected'".to_owned());
        }
        let predicate = if predicates.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", predicates.join(" AND "))
        };
        let total = connection
            .query_row(
                &format!("SELECT COUNT(*) FROM relation_groups{predicate}"),
                [],
                |row| row.get::<_, u64>(0),
            )
            .map_err(|error| storage_error("RELATION_GROUP_QUERY_FAILED", error, true))?;
        let raw = {
            let mut statement = connection
                .prepare(&format!(
                    "SELECT group_id, group_type, title, confidence, member_count, review_status, created_at, updated_at FROM relation_groups{predicate} ORDER BY confidence DESC, updated_at DESC, group_id DESC LIMIT ?1 OFFSET ?2"
                ))
                .map_err(|error| storage_error("RELATION_GROUP_QUERY_FAILED", error, true))?;
            statement
                .query_map(params![page_size, offset], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, f64>(3)?,
                        row.get::<_, u32>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, String>(7)?,
                    ))
                })
                .map_err(|error| storage_error("RELATION_GROUP_QUERY_FAILED", error, true))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| storage_error("RELATION_GROUP_QUERY_FAILED", error, true))?
        };
        let mut items = Vec::new();
        for (
            group_id,
            group_type,
            title,
            confidence,
            member_count,
            review_status,
            created_at,
            updated_at,
        ) in raw
        {
            let group_id_uuid = parse_uuid_value(&group_id)?;
            let mut members = Vec::new();
            {
                let mut statement = connection
                    .prepare(
                        "SELECT file_id, role FROM relation_group_members WHERE group_id = ?1 ORDER BY rowid",
                    )
                    .map_err(|error| {
                        storage_error("RELATION_GROUP_QUERY_FAILED", error, true)
                    })?;
                let rows = statement
                    .query_map([&group_id], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })
                    .map_err(|error| storage_error("RELATION_GROUP_QUERY_FAILED", error, true))?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|error| storage_error("RELATION_GROUP_QUERY_FAILED", error, true))?;
                for (file_id, role) in rows {
                    let file_id = parse_uuid_value(&file_id)?;
                    members.push(RelationGroupMemberRecord {
                        file_id,
                        role: RelationGroupRole::from_storage(&role),
                        file: authorized_file_by_id(&connection, &file_id)?,
                    });
                }
            }
            items.push(RelationGroupRecord {
                group_id: group_id_uuid,
                group_type: RelationGroupType::from_storage(&group_type),
                title,
                confidence,
                member_count,
                review_status,
                created_at: DateTime::parse_from_rfc3339(&created_at)
                    .map_err(|error| {
                        AppError::new("RELATION_GROUP_DATA_INVALID", error.to_string(), false)
                    })?
                    .with_timezone(&Utc),
                updated_at: DateTime::parse_from_rfc3339(&updated_at)
                    .map_err(|error| {
                        AppError::new("RELATION_GROUP_DATA_INVALID", error.to_string(), false)
                    })?
                    .with_timezone(&Utc),
                members,
            });
        }
        let consumed = offset.saturating_add(items.len() as u64);
        Ok(RelationGroupPage {
            items,
            next_cursor: (consumed < total).then(|| consumed.to_string()),
            total,
        })
    }

    /// 组级复核：把组内所有同状态边批量复核，并同步组的复核状态。
    pub fn review_relation_group(&self, group_id: &Uuid, action: &str) -> Result<(), AppError> {
        if !matches!(action, "accepted" | "rejected") {
            return Err(AppError::new(
                "RELATION_GROUP_REVIEW_INVALID",
                "关系组复核动作只能是 accepted 或 rejected",
                false,
            ));
        }
        let _permit = self.acquire_write(WritePriority::Interactive);
        let connection = self.connect()?;
        let member_ids = {
            let mut statement = connection
                .prepare("SELECT file_id FROM relation_group_members WHERE group_id = ?1")
                .map_err(|error| storage_error("RELATION_GROUP_REVIEW_FAILED", error, true))?;
            statement
                .query_map([group_id.to_string()], |row| row.get::<_, String>(0))
                .map_err(|error| storage_error("RELATION_GROUP_REVIEW_FAILED", error, true))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| storage_error("RELATION_GROUP_REVIEW_FAILED", error, true))?
        };
        if member_ids.len() < 2 {
            return Err(AppError::new(
                "RELATION_GROUP_REVIEW_INVALID",
                "待复核的关系组不存在",
                false,
            ));
        }
        let placeholders = std::iter::repeat_n("?", member_ids.len())
            .collect::<Vec<_>>()
            .join(",");
        let mut values = Vec::with_capacity(member_ids.len() * 2 + 2);
        values.push(SqlValue::Text(action.to_owned()));
        values.push(SqlValue::Text(Utc::now().to_rfc3339()));
        values.extend(member_ids.iter().map(|id| SqlValue::Text(id.clone())));
        values.extend(member_ids.iter().map(|id| SqlValue::Text(id.clone())));
        let updated = connection
            .execute(
                &format!(
                    "UPDATE file_relations SET review_status = ?, updated_at = ? WHERE review_status <> 'rejected' AND left_file_id IN ({placeholders}) AND right_file_id IN ({placeholders}) AND EXISTS (SELECT 1 FROM file_root_memberships m JOIN roots rt ON rt.root_id = m.root_id WHERE m.file_id = left_file_id AND rt.enabled = 1) AND EXISTS (SELECT 1 FROM file_root_memberships m JOIN roots rt ON rt.root_id = m.root_id WHERE m.file_id = right_file_id AND rt.enabled = 1)"
                ),
                params_from_iter(values),
            )
            .map_err(|error| storage_error("RELATION_GROUP_REVIEW_FAILED", error, true))?;
        if updated == 0 {
            return Err(AppError::new(
                "RELATION_GROUP_REVIEW_INVALID",
                "关系组内没有可复核的边",
                false,
            ));
        }
        connection
            .execute(
                "UPDATE relation_groups SET review_status = ?1, updated_at = ?2 WHERE group_id = ?3 AND review_status <> 'rejected'",
                params![action, Utc::now().to_rfc3339(), group_id.to_string()],
            )
            .map_err(|error| storage_error("RELATION_GROUP_REVIEW_FAILED", error, true))?;
        Ok(())
    }

    /// 组级批量复核。
    pub fn review_relation_groups(
        &self,
        group_ids: &[Uuid],
        action: &str,
    ) -> Result<u64, AppError> {
        if group_ids.is_empty() || group_ids.len() > 500 {
            return Err(AppError::new(
                "RELATION_GROUP_REVIEW_INVALID",
                "每次批量复核需要选择1到500个关系组",
                false,
            ));
        }
        if !matches!(action, "accepted" | "rejected") {
            return Err(AppError::new(
                "RELATION_GROUP_REVIEW_INVALID",
                "关系组复核动作只能是 accepted 或 rejected",
                false,
            ));
        }
        let _permit = self.acquire_write(WritePriority::Interactive);
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction()
            .map_err(|error| storage_error("RELATION_GROUP_REVIEW_FAILED", error, true))?;
        let updated_at = Utc::now().to_rfc3339();
        let mut changed = 0_u64;
        for group_id in group_ids.iter().collect::<HashSet<_>>() {
            let member_ids = {
                let mut statement = transaction
                    .prepare("SELECT file_id FROM relation_group_members WHERE group_id = ?1")
                    .map_err(|error| storage_error("RELATION_GROUP_REVIEW_FAILED", error, true))?;
                statement
                    .query_map([group_id.to_string()], |row| row.get::<_, String>(0))
                    .map_err(|error| storage_error("RELATION_GROUP_REVIEW_FAILED", error, true))?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|error| storage_error("RELATION_GROUP_REVIEW_FAILED", error, true))?
            };
            if member_ids.len() < 2 {
                continue;
            }
            let placeholders = std::iter::repeat_n("?", member_ids.len())
                .collect::<Vec<_>>()
                .join(",");
            let mut values = Vec::with_capacity(member_ids.len() * 2 + 2);
            values.push(SqlValue::Text(action.to_owned()));
            values.push(SqlValue::Text(updated_at.clone()));
            values.extend(member_ids.iter().map(|id| SqlValue::Text(id.clone())));
            values.extend(member_ids.iter().map(|id| SqlValue::Text(id.clone())));
            changed = changed.saturating_add(
                transaction
                    .execute(
                        &format!(
                            "UPDATE file_relations SET review_status = ?, updated_at = ? WHERE review_status <> 'rejected' AND left_file_id IN ({placeholders}) AND right_file_id IN ({placeholders})"
                        ),
                        params_from_iter(values),
                    )
                    .map_err(|error| {
                        storage_error("RELATION_GROUP_REVIEW_FAILED", error, true)
                    })?
                    as u64,
            );
            transaction
                .execute(
                    "UPDATE relation_groups SET review_status = ?1, updated_at = ?2 WHERE group_id = ?3 AND review_status <> 'rejected'",
                    params![action, updated_at, group_id.to_string()],
                )
                .map_err(|error| storage_error("RELATION_GROUP_REVIEW_FAILED", error, true))?;
        }
        if changed == 0 {
            return Err(AppError::new(
                "RELATION_GROUP_REVIEW_INVALID",
                "所选关系组内没有可复核的边",
                false,
            ));
        }
        transaction
            .commit()
            .map_err(|error| storage_error("RELATION_GROUP_REVIEW_FAILED", error, true))?;
        Ok(changed)
    }

    pub fn list_pending_parse_files(&self, limit: usize) -> Result<Vec<FileRecord>, AppError> {
        let connection = self.connect()?;
        let mut statement = connection
            .prepare(
                "SELECT file_id, volume_id, canonical_path, display_name, extension, mime_type, size_bytes, fs_created_at, modified_at, windows_file_id, content_sha256, availability, current_revision_id, parse_status, first_seen_at, last_seen_at FROM files WHERE availability = 'present' AND current_revision_id IS NOT NULL AND parse_status = 'pending' AND extension IN ('pdf', 'docx', 'docm', 'xlsx', 'xlsm', 'pptx', 'pptm', 'csv', 'tsv', 'md', 'txt', 'text', 'ini', 'iml', 'log', 'conf', 'cfg', 'properties', 'html', 'htm', 'jpg', 'jpeg', 'png', 'tif', 'tiff', 'bmp', 'webp', 'doc', 'xls', 'ppt', 'zip', 'rs', 'py', 'js', 'jsx', 'mjs', 'cjs', 'ts', 'tsx', 'java', 'kt', 'kts', 'go', 'c', 'cc', 'cpp', 'h', 'hpp', 'cs', 'rb', 'php', 'swift', 'scala', 'sh', 'ps1', 'sql', 'json', 'yaml', 'yml', 'toml', 'xml', 'css', 'scss', 'vue', 'svelte') ORDER BY last_seen_at, file_id LIMIT ?1",
            )
            .map_err(|error| storage_error("FILE_QUERY_FAILED", error, true))?;
        statement
            .query_map([limit as u64], file_from_row)
            .map_err(|error| storage_error("FILE_QUERY_FAILED", error, true))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| storage_error("FILE_QUERY_FAILED", error, true))
    }

    /// 只领取 ocr_pending 文件的图片理解资产,供 VLM 消费端处理。
    /// 与 claim_pending_image_understanding 的区别:附带 f.parse_status='ocr_pending' 过滤,
    /// 避免把已 parsed 文件的图片理解资产(纯增强)也拉进评测必需的 OCR 补齐流程。
    pub fn claim_pending_ocr_image_understanding(
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
            "SELECT ia.asset_id, ia.file_id, ia.revision_id, ia.cache_path, ia.mime_type, ia.size_bytes, ia.sha256, ia.locator_json, ia.ocr_text, ia.attempt_count FROM image_assets ia JOIN files f ON f.file_id = ia.file_id WHERE ia.status = 'pending_understanding' AND f.current_revision_id = ia.revision_id AND f.availability = 'present' AND f.parse_status = 'ocr_pending' AND {AUTHORIZED_FILE_SQL} ORDER BY ia.updated_at, ia.asset_id LIMIT 1"
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

    /// 文件级翻转:当 ocr_pending 文件的全部图片资产都已决(至少一个 ready、且无
    /// pending_understanding/processing 在途),把文件提升为 parsed。
    /// 全部资产 failed 的文件不翻转——VLM 未能补全内容,仍应停留在 ocr_pending 可重试。
    /// 列出当前所有 ocr_pending 文件（用于批量提升扫描，如 --promote-only）。
    pub fn list_ocr_pending_files(&self) -> Result<Vec<Uuid>, AppError> {
        let connection = self.connect()?;
        let mut statement = connection
            .prepare(
                "SELECT file_id FROM files
                 WHERE availability = 'present'
                   AND current_revision_id IS NOT NULL
                   AND parse_status = 'ocr_pending'
                   AND EXISTS (
                     SELECT 1 FROM file_root_memberships frm
                     JOIN roots r ON r.root_id = frm.root_id
                     WHERE frm.file_id = files.file_id AND r.enabled = 1
                   )
                 ORDER BY last_seen_at, file_id",
            )
            .map_err(|error| storage_error("OCR_PENDING_LIST_FAILED", error, true))?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|error| storage_error("OCR_PENDING_LIST_FAILED", error, true))?;
        let mut file_ids = Vec::new();
        for row in rows {
            let raw = row.map_err(|error| storage_error("OCR_PENDING_LIST_FAILED", error, true))?;
            file_ids.push(parse_uuid_value(&raw)?);
        }
        Ok(file_ids)
    }

    pub fn promote_ocr_pending_file_when_assets_ready(
        &self,
        file_id: &Uuid,
    ) -> Result<bool, AppError> {
        let _permit = self.acquire_write(WritePriority::Background);
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction()
            .map_err(|error| storage_error("OCR_PROMOTE_UPDATE_FAILED", error, true))?;
        let sql = format!(
            "UPDATE files AS f SET parse_status = 'parsed', processing_reason_code = NULL WHERE f.file_id = ?1 AND f.parse_status = 'ocr_pending' AND f.availability = 'present' AND f.current_revision_id IS NOT NULL AND EXISTS (SELECT 1 FROM image_assets ia WHERE ia.file_id = ?1 AND ia.revision_id = f.current_revision_id AND ia.status = 'ready') AND NOT EXISTS (SELECT 1 FROM image_assets ia WHERE ia.file_id = ?1 AND ia.revision_id = f.current_revision_id AND ia.status IN ('pending_ocr', 'ocr_processing', 'pending_understanding', 'processing')) AND {AUTHORIZED_FILE_SQL}"
        );
        let changed = transaction
            .execute(&sql, [file_id.to_string()])
            .map_err(|error| storage_error("OCR_PROMOTE_UPDATE_FAILED", error, true))?;
        if changed == 1 {
            transaction
                .execute(
                    "UPDATE file_revisions SET parse_status = 'parsed', error_code = NULL, completed_at = ?1 WHERE revision_id = (SELECT current_revision_id FROM files WHERE file_id = ?2)",
                    params![Utc::now().to_rfc3339(), file_id.to_string()],
                )
                .map_err(|error| storage_error("OCR_PROMOTE_UPDATE_FAILED", error, true))?;
            transaction
                .execute(
                    "UPDATE inbox_events SET resolution_status = 'resolved' WHERE file_id = ?1 AND event_type = 'ocr_required' AND resolution_status IN ('normal', 'pending_retry', 'retrying')",
                    [file_id.to_string()],
                )
                .map_err(|error| storage_error("OCR_PROMOTE_UPDATE_FAILED", error, true))?;
        }
        transaction
            .commit()
            .map_err(|error| storage_error("DATABASE_COMMIT_FAILED", error, true))?;
        Ok(changed == 1)
    }

    pub fn retry_ocr(&self, file_id: &Uuid) -> Result<(), AppError> {
        let _permit = self.acquire_write(WritePriority::Interactive);
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

    pub fn requeue_ocr_pending_for_available_runtime(&self, limit: usize) -> Result<u64, AppError> {
        if !(1..=5_000).contains(&limit) {
            return Err(AppError::new(
                "OCR_REQUEUE_LIMIT_INVALID",
                "OCR恢复批量大小必须在1到5000之间",
                false,
            ));
        }
        let _permit = self.acquire_write(WritePriority::Background);
        let connection = self.connect()?;
        let changed = connection
            .execute(
                "WITH eligible AS (
                    SELECT f.file_id
                    FROM files f
                    WHERE f.availability = 'present'
                      AND f.current_revision_id IS NOT NULL
                      AND f.parse_status = 'ocr_pending'
                      AND EXISTS (
                        SELECT 1
                        FROM file_root_memberships frm
                        JOIN roots r ON r.root_id = frm.root_id
                        WHERE frm.file_id = f.file_id AND r.enabled = 1
                      )
                      -- 配额 6（原为 3）：早期 Windows OCR 引擎不可用时的失败
                      -- 尝试不应耗尽新引擎（PP-OCRv5）的回归机会——引擎就绪前
                      -- 的尝试不计数，给足轮次；超过 6 次的损坏文件仍会停止重试。
                      AND (
                        SELECT COUNT(*)
                        FROM processing_attempts pa
                        WHERE pa.file_id = f.file_id
                          AND pa.operation = 'parse'
                          AND pa.status = 'ocr_pending'
                      ) < 6
                    ORDER BY f.last_seen_at, f.file_id
                    LIMIT ?1
                )
                UPDATE files
                SET parse_status = 'pending', processing_reason_code = NULL
                WHERE file_id IN (SELECT file_id FROM eligible)",
                [limit as u64],
            )
            .map_err(|error| storage_error("OCR_REQUEUE_FAILED", error, true))?;
        Ok(changed as u64)
    }

    pub fn sanitize_existing_ocr_attempt_errors(&self) -> Result<u64, AppError> {
        let _permit = self.acquire_write(WritePriority::Background);
        let mut connection = self.connect()?;
        let rows = {
            let mut statement = connection
                .prepare(
                    "SELECT attempt_id, error_json FROM ocr_attempts WHERE error_json IS NOT NULL",
                )
                .map_err(|error| storage_error("OCR_ATTEMPT_QUERY_FAILED", error, true))?;
            statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(|error| storage_error("OCR_ATTEMPT_QUERY_FAILED", error, true))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| storage_error("OCR_ATTEMPT_QUERY_FAILED", error, true))?
        };
        let transaction = connection
            .transaction()
            .map_err(|error| storage_error("OCR_ATTEMPT_SANITIZE_FAILED", error, true))?;
        let mut changed = 0_u64;
        for (attempt_id, raw_error) in rows {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw_error) else {
                continue;
            };
            let sanitized = serde_json::to_string(&sanitize_log_value(&value, None))
                .map_err(|error| AppError::new("OCR_ATTEMPT_INVALID", error.to_string(), false))?;
            if sanitized == raw_error {
                continue;
            }
            changed = changed.saturating_add(
                transaction
                    .execute(
                        "UPDATE ocr_attempts SET error_json = ?1 WHERE attempt_id = ?2",
                        params![sanitized, attempt_id],
                    )
                    .map_err(|error| storage_error("OCR_ATTEMPT_SANITIZE_FAILED", error, true))?
                    as u64,
            );
        }
        transaction
            .commit()
            .map_err(|error| storage_error("OCR_ATTEMPT_SANITIZE_FAILED", error, true))?;
        Ok(changed)
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
        let _permit = self.acquire_write(WritePriority::Background);
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

    /// 统计指定 Embedding 模型在当前索引代上的重建进度：
    /// 返回 (已用该模型嵌入的分块数, 可搜索分块总数)。
    /// 重建换模型后，旧模型向量按 `(chunk_id, model_artifact_id)` 存储不会被计入，
    /// 因此 `done` 从 0 逐步逼近 `total`，可可靠反映「语义索引重建」的真实进度。
    pub fn embedding_rebuild_progress(
        &self,
        model_artifact_id: &str,
    ) -> Result<(u64, u64), AppError> {
        let connection = self.connect()?;
        let done = connection
            .query_row(
                "SELECT COUNT(*) FROM chunk_embeddings WHERE model_artifact_id = ?1",
                params![model_artifact_id],
                |row| row.get::<_, u64>(0),
            )
            .map_err(|error| storage_error("DATABASE_COUNT_QUERY_FAILED", error, true))?;
        let total = count_query(&connection, "SELECT COUNT(*) FROM chunks")?;
        Ok((done, total))
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

    /// 是否存在任何已激活的向量索引代际（不限模型）。
    /// 用于检测「Embedding 换代但新模型尚未建立索引代」的提示场景；只读，不触碰索引数据。
    pub fn any_active_vector_generation(&self) -> Result<bool, AppError> {
        let connection = self.connect()?;
        let exists: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM index_generations WHERE status = 'active')",
                [],
                |row| row.get::<_, bool>(0),
            )
            .map_err(|error| storage_error("VECTOR_INDEX_GENERATION_QUERY_FAILED", error, true))?;
        Ok(exists)
    }

    /// 当前已激活的向量索引代所采用的 embedding 模型 artifact id（无则 `None`）。
    /// 供 `index_stale_check` 比较「当前激活 Embedding」与「索引实际使用的 Embedding」
    /// 是否一致，从而提示是否需要重建向量索引；只读，不触碰索引文件。
    pub fn active_index_model_artifact_id(&self) -> Result<Option<String>, AppError> {
        let connection = self.connect()?;
        connection
            .query_row(
                "SELECT model_artifact_id FROM index_generations WHERE status = 'active' LIMIT 1",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| storage_error("VECTOR_INDEX_GENERATION_QUERY_FAILED", error, true))
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
        {
            let _permit = self.acquire_write(WritePriority::Background);
            let connection = self.connect()?;
            connection
                .execute(
                    "INSERT INTO index_generations (generation_id, model_artifact_id, dimension, metric, quantization, index_path, status, item_count, coverage, created_at) VALUES (?1, ?2, ?3, 'cosine', 'bf16', ?4, 'building', 0, 0, ?5)",
                    params![generation_id.to_string(), model_artifact_id, dimension, index_path.to_string_lossy(), created_at.to_rfc3339()],
                )
                .map_err(|error| storage_error("VECTOR_INDEX_GENERATION_WRITE_FAILED", error, true))?;
        }
        let connection = self.connect()?;

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
        drop(connection);
        for batch in loaded.chunks(500) {
            let _permit = self.acquire_write(WritePriority::Background);
            let mut connection = self.connect()?;
            let transaction = connection
                .transaction()
                .map_err(|error| storage_error("DATABASE_TRANSACTION_FAILED", error, true))?;
            for (key, chunk_id, file_id, revision_id, _) in batch {
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
                    "UPDATE index_generations SET item_count = ?2 WHERE generation_id = ?1 AND status = 'building'",
                    params![generation_id.to_string(), batch.len() as u64],
                )
                .map_err(|error| storage_error("VECTOR_INDEX_GENERATION_WRITE_FAILED", error, true))?;
            transaction
                .commit()
                .map_err(|error| storage_error("DATABASE_COMMIT_FAILED", error, true))?;
            drop(_permit);
            std::thread::yield_now();
        }
        let _permit = self.acquire_write(WritePriority::Background);
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction()
            .map_err(|error| storage_error("DATABASE_TRANSACTION_FAILED", error, true))?;
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
        let _permit = self.acquire_write(WritePriority::Background);
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
                    "pending_ocr" | "pending_understanding" | "ready" | "failed"
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
        let _permit = self.acquire_write(WritePriority::Background);
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
        transaction
            .execute(
                "DELETE FROM ocr_attempts WHERE revision_id = ?1",
                [result.revision_id.to_string()],
            )
            .map_err(|error| storage_error("OCR_ATTEMPT_WRITE_FAILED", error, true))?;

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
                    "INSERT INTO image_assets (asset_id, file_id, revision_id, asset_kind, cache_path, mime_type, size_bytes, sha256, locator_json, ocr_text, ocr_confidence, ocr_engine, description, vision_model_id, vision_route_reason, status, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?17)",
                    params![asset.asset_id.to_string(), file_id.to_string(), asset.revision_id.to_string(), asset.asset_kind, asset.cache_path, asset.mime_type, asset.size_bytes, asset.sha256, locator_json, asset.ocr_text, asset.ocr_confidence, asset.ocr_engine, asset.description, asset.vision_model_id, asset.vision_route_reason, asset.status, now],
                )
                .map_err(|error| storage_error("IMAGE_ASSET_WRITE_FAILED", error, true))?;
            if asset.status == "ready"
                && let Some(ocr_text) = asset
                    .ocr_text
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
            {
                replace_image_search_node(
                    &transaction,
                    file_id,
                    &asset.revision_id,
                    &asset.asset_id,
                    &asset.locator,
                    "image_ocr",
                    "图片文字",
                    format!("图片文字：{ocr_text}"),
                )?;
            }
        }
        for (ordinal, attempt) in result.ocr_attempts.iter().enumerate() {
            let error_json = attempt
                .error
                .as_ref()
                .map(|error| {
                    serde_json::to_value(error)
                        .map(|value| sanitize_log_value(&value, None))
                        .and_then(|value| serde_json::to_string(&value))
                })
                .transpose()
                .map_err(|error| AppError::new("OCR_ATTEMPT_INVALID", error.to_string(), false))?;
            transaction
                .execute(
                    "INSERT INTO ocr_attempts (attempt_id, revision_id, ordinal, engine, model_version, status, page_no, confidence, fallback_reason, elapsed_ms, error_json, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                    params![
                        Uuid::now_v7().to_string(),
                        result.revision_id.to_string(),
                        ordinal as u64,
                        &attempt.engine,
                        attempt.model_version.as_deref(),
                        &attempt.status,
                        attempt.page_no,
                        attempt.confidence,
                        attempt.fallback_reason.as_deref(),
                        attempt.elapsed_ms,
                        error_json,
                        Utc::now().to_rfc3339(),
                    ],
                )
                .map_err(|error| storage_error("OCR_ATTEMPT_WRITE_FAILED", error, true))?;
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
        // files.processing_reason_code 只记录「确有异常/待处理」的状态：
        // 解析成功（parsed）时即便带尽力而为告警（如 PDF_IMAGE_EXTRACT_FAILED
        // 这类图片提取跳过），也不写入——否则会留下误导性的报错残留码。
        let reason_code = match parse_status {
            "parsed" => None,
            _ => error_code,
        };
        let now = Utc::now().to_rfc3339();
        let attempt_no = transaction
            .query_row(
                "SELECT COUNT(*) + 1 FROM processing_attempts WHERE revision_id = ?1 AND operation = 'parse'",
                [result.revision_id.to_string()],
                |row| row.get::<_, u64>(0),
            )
            .map_err(|error| storage_error("PROCESSING_ATTEMPT_QUERY_FAILED", error, true))?;
        let safe_error_json = result
            .error
            .as_ref()
            .map(serde_json::to_value)
            .transpose()
            .map_err(|error| AppError::new("PROCESSING_ATTEMPT_INVALID", error.to_string(), false))?
            .map(|value| sanitize_log_value(&value, None))
            .map(|value| value.to_string());
        transaction
            .execute(
                "INSERT INTO processing_attempts (attempt_id, file_id, revision_id, operation, engine, model_version, status, attempt_no, elapsed_ms, retryable, error_json, created_at) VALUES (?1, ?2, ?3, 'parse', ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    Uuid::now_v7().to_string(),
                    file_id.to_string(),
                    result.revision_id.to_string(),
                    result.parser_name,
                    result.parser_version,
                    parse_status,
                    attempt_no,
                    result.metrics.elapsed_ms,
                    result.error.as_ref().is_some_and(|error| error.retryable),
                    safe_error_json,
                    now,
                ],
            )
            .map_err(|error| storage_error("PROCESSING_ATTEMPT_WRITE_FAILED", error, true))?;
        transaction
            .execute(
                "UPDATE file_revisions SET parse_status = ?1, parser_name = ?2, parser_version = ?3, index_version = ?4, completed_at = ?5, error_code = ?6 WHERE revision_id = ?7",
                params![parse_status, result.parser_name, result.parser_version, crate::INDEX_VERSION, now, error_code, result.revision_id.to_string()],
            )
            .map_err(|error| storage_error("INDEX_STATE_UPDATE_FAILED", error, true))?;
        transaction
            .execute(
                "UPDATE files SET parse_status = ?1, processing_reason_code = ?2, processing_disposition = CASE WHEN ?1 = 'encrypted' THEN 'encrypted_or_damaged' ELSE processing_disposition END WHERE file_id = ?3 AND current_revision_id = ?4",
                params![parse_status, reason_code, file_id.to_string(), result.revision_id.to_string()],
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
            let triage_status = TriageStatus::New;
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
                    "UPDATE inbox_events SET resolution_status = 'resolved' WHERE file_id = ?1 AND event_type IN ('ocr_required','parse_failed') AND resolution_status IN ('pending_retry','retrying')",
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

    pub fn recover_interrupted_image_ocr(&self) -> Result<u64, AppError> {
        let connection = self.connect()?;
        connection
            .execute(
                "UPDATE image_assets SET status = 'pending_ocr', updated_at = ?1, ocr_error_json = ?2 WHERE status = 'ocr_processing'",
                params![Utc::now().to_rfc3339(), serde_json::to_string(&AppError::new("IMAGE_OCR_INTERRUPTED", "应用上次退出时图片OCR尚未完成，已从检查点恢复", true)).expect("serialize static error")],
            )
            .map(|changed| changed as u64)
            .map_err(|error| storage_error("IMAGE_OCR_RECOVERY_FAILED", error, true))
    }

    pub fn backfill_ready_image_search_nodes(&self, limit: usize) -> Result<Vec<Uuid>, AppError> {
        if !(1..=5_000).contains(&limit) {
            return Err(AppError::new(
                "IMAGE_SEARCH_BACKFILL_LIMIT_INVALID",
                "图片检索回填批量大小必须在1到5000之间",
                false,
            ));
        }
        let _permit = self.acquire_write(WritePriority::Background);
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction()
            .map_err(|error| storage_error("IMAGE_SEARCH_BACKFILL_FAILED", error, true))?;
        let sql = format!(
            "SELECT ia.asset_id, ia.file_id, ia.revision_id, ia.locator_json, ia.ocr_text, ia.description FROM image_assets ia JOIN files f ON f.file_id = ia.file_id WHERE ia.status = 'ready' AND f.current_revision_id = ia.revision_id AND f.availability = 'present' AND COALESCE(NULLIF(TRIM(ia.description), ''), NULLIF(TRIM(ia.ocr_text), '')) IS NOT NULL AND NOT EXISTS (SELECT 1 FROM document_nodes dn WHERE dn.image_asset_id = ia.asset_id) AND {AUTHORIZED_FILE_SQL} ORDER BY ia.updated_at, ia.asset_id LIMIT ?1"
        );
        let rows = {
            let mut statement = transaction
                .prepare(&sql)
                .map_err(|error| storage_error("IMAGE_SEARCH_BACKFILL_FAILED", error, true))?;
            statement
                .query_map([limit as u64], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Option<String>>(5)?,
                    ))
                })
                .map_err(|error| storage_error("IMAGE_SEARCH_BACKFILL_FAILED", error, true))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| storage_error("IMAGE_SEARCH_BACKFILL_FAILED", error, true))?
        };
        let mut file_ids = Vec::new();
        for (asset_id, file_id, revision_id, locator_json, ocr_text, description) in rows {
            let asset_id = parse_uuid_value(&asset_id)?;
            let file_id = parse_uuid_value(&file_id)?;
            let revision_id = parse_uuid_value(&revision_id)?;
            let locator = serde_json::from_str::<SourceLocator>(&locator_json)
                .map_err(|error| AppError::new("IMAGE_ASSET_INVALID", error.to_string(), false))?;
            let description = description
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty());
            let ocr_text = ocr_text
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty());
            let (node_type, heading, text) = if let Some(description) = description {
                let mut text = format!("图片说明：{description}");
                if let Some(ocr_text) = ocr_text {
                    text.push_str("\n可见文字：");
                    text.push_str(ocr_text);
                }
                ("image_description", "图片理解", text)
            } else {
                (
                    "image_ocr",
                    "图片文字",
                    format!("图片文字：{}", ocr_text.expect("validated image text")),
                )
            };
            replace_image_search_node(
                &transaction,
                &file_id,
                &revision_id,
                &asset_id,
                &locator,
                node_type,
                heading,
                text,
            )?;
            if !file_ids.contains(&file_id) {
                file_ids.push(file_id);
            }
        }
        transaction
            .commit()
            .map_err(|error| storage_error("IMAGE_SEARCH_BACKFILL_FAILED", error, true))?;
        Ok(file_ids)
    }

    pub fn claim_pending_image_ocr(
        &self,
        model_artifact_id: &str,
    ) -> Result<Option<PendingImageOcr>, AppError> {
        if model_artifact_id.trim().is_empty() {
            return Err(AppError::new(
                "OCR_MODEL_INVALID",
                "图片OCR任务缺少模型标识",
                false,
            ));
        }
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction()
            .map_err(|error| storage_error("IMAGE_OCR_CLAIM_FAILED", error, true))?;
        let sql = format!(
            "SELECT ia.asset_id, ia.file_id, ia.revision_id, ia.cache_path, ia.mime_type, ia.asset_kind, ia.size_bytes, ia.sha256, ia.locator_json, ia.ocr_attempt_count FROM image_assets ia JOIN files f ON f.file_id = ia.file_id WHERE ia.status = 'pending_ocr' AND f.current_revision_id = ia.revision_id AND f.availability = 'present' AND {AUTHORIZED_FILE_SQL} ORDER BY ia.updated_at, ia.asset_id LIMIT 1"
        );
        let row = transaction
            .query_row(&sql, [], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, u64>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, u32>(9)?,
                ))
            })
            .optional()
            .map_err(|error| storage_error("IMAGE_OCR_CLAIM_FAILED", error, true))?;
        let Some((
            asset_id,
            file_id,
            revision_id,
            cache_path,
            mime_type,
            asset_kind,
            size_bytes,
            sha256,
            locator_json,
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
                    "UPDATE image_assets SET status = 'failed', ocr_error_json = ?1, updated_at = ?2 WHERE asset_id = ?3",
                    params![serde_json::to_string(&AppError::new("IMAGE_ASSET_CACHE_INVALID", "图片缓存大小或哈希已经变化，需要重新解析源文件", false)).expect("serialize static error"), Utc::now().to_rfc3339(), asset_id.to_string()],
                )
                .map_err(|error| storage_error("IMAGE_OCR_CLAIM_FAILED", error, true))?;
            transaction
                .commit()
                .map_err(|error| storage_error("DATABASE_COMMIT_FAILED", error, true))?;
            return Ok(None);
        }
        let idempotency_key = format!("image-ocr:v1:{model_artifact_id}:{sha256}");
        let now = Utc::now().to_rfc3339();
        let changed = transaction
            .execute(
                "UPDATE image_assets SET status = 'ocr_processing', ocr_attempt_count = ocr_attempt_count + 1, ocr_error_json = NULL, ocr_idempotency_key = ?1, updated_at = ?2 WHERE asset_id = ?3 AND status = 'pending_ocr'",
                params![idempotency_key, now, asset_id.to_string()],
            )
            .map_err(|error| storage_error("IMAGE_OCR_CLAIM_FAILED", error, true))?;
        if changed != 1 {
            return Err(AppError::new(
                "IMAGE_OCR_ALREADY_CLAIMED",
                "图片OCR任务已由另一个后台执行器领取",
                true,
            ));
        }
        transaction
            .commit()
            .map_err(|error| storage_error("DATABASE_COMMIT_FAILED", error, true))?;
        Ok(Some(PendingImageOcr {
            asset_id,
            file_id,
            revision_id,
            cache_path,
            mime_type,
            asset_kind,
            size_bytes,
            sha256,
            locator,
            attempt_count: attempt_count.saturating_add(1),
            idempotency_key,
        }))
    }

    pub fn commit_image_ocr(&self, result: &ImageOcrResult) -> Result<(), AppError> {
        let ocr_text = result
            .ocr_text
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        if result.engine.trim().is_empty()
            || result.route_reason.trim().is_empty()
            || result.idempotency_key.trim().is_empty()
            || result
                .confidence
                .is_some_and(|value| !value.is_finite() || !(0.0..=1.0).contains(&value))
            || (!result.vision_required && ocr_text.is_none())
        {
            return Err(AppError::new(
                "IMAGE_OCR_RESULT_INVALID",
                "图片OCR结果、置信度或路由结论无效",
                false,
            ));
        }
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction()
            .map_err(|error| storage_error("IMAGE_OCR_COMMIT_FAILED", error, true))?;
        let sql = format!(
            "SELECT ia.file_id, ia.revision_id, ia.locator_json, ia.status, ia.ocr_idempotency_key FROM image_assets ia JOIN files f ON f.file_id = ia.file_id WHERE ia.asset_id = ?1 AND f.current_revision_id = ia.revision_id AND f.availability = 'present' AND {AUTHORIZED_FILE_SQL} LIMIT 1"
        );
        let row = transaction
            .query_row(&sql, [result.asset_id.to_string()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            })
            .optional()
            .map_err(|error| storage_error("IMAGE_OCR_COMMIT_FAILED", error, true))?
            .ok_or_else(|| {
                AppError::new(
                    "IMAGE_OCR_STALE_REVISION",
                    "图片所属文件已经变化、离线或超出授权范围，OCR结果未写入",
                    false,
                )
            })?;
        let (file_id, revision_id, locator_json, status, stored_key) = row;
        let revision_id = parse_uuid_value(&revision_id)?;
        if revision_id != result.revision_id {
            return Err(AppError::new(
                "IMAGE_OCR_STALE_REVISION",
                "图片OCR结果不属于当前文件修订",
                false,
            ));
        }
        if matches!(status.as_str(), "ready" | "pending_understanding")
            && stored_key.as_deref() == Some(result.idempotency_key.as_str())
        {
            return Ok(());
        }
        if status != "ocr_processing"
            || stored_key.as_deref() != Some(result.idempotency_key.as_str())
        {
            return Err(AppError::new(
                "IMAGE_OCR_CHECKPOINT_MISMATCH",
                "图片OCR任务检查点已经变化，结果未写入",
                true,
            ));
        }
        let file_id = parse_uuid_value(&file_id)?;
        let locator = serde_json::from_str::<SourceLocator>(&locator_json)
            .map_err(|error| AppError::new("IMAGE_ASSET_INVALID", error.to_string(), false))?;
        let attempt_base = transaction
            .query_row(
                "SELECT COUNT(*) FROM ocr_attempts WHERE revision_id = ?1",
                [revision_id.to_string()],
                |row| row.get::<_, u64>(0),
            )
            .map_err(|error| storage_error("OCR_ATTEMPT_QUERY_FAILED", error, true))?;
        for (offset, attempt) in result.attempts.iter().enumerate() {
            let error_json = attempt
                .error
                .as_ref()
                .map(serde_json::to_value)
                .transpose()
                .map_err(|error| AppError::new("OCR_ATTEMPT_INVALID", error.to_string(), false))?
                .map(|value| sanitize_log_value(&value, None).to_string());
            transaction
                .execute(
                    "INSERT INTO ocr_attempts (attempt_id, revision_id, ordinal, engine, model_version, status, page_no, confidence, fallback_reason, elapsed_ms, error_json, created_at, image_asset_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                    params![Uuid::now_v7().to_string(), revision_id.to_string(), attempt_base + offset as u64, attempt.engine, attempt.model_version, attempt.status, attempt.page_no, attempt.confidence, attempt.fallback_reason, attempt.elapsed_ms, error_json, Utc::now().to_rfc3339(), result.asset_id.to_string()],
                )
                .map_err(|error| storage_error("OCR_ATTEMPT_WRITE_FAILED", error, true))?;
        }
        if !result.vision_required {
            replace_image_search_node(
                &transaction,
                &file_id,
                &revision_id,
                &result.asset_id,
                &locator,
                "image_ocr",
                "图片文字",
                format!("图片文字：{}", ocr_text.expect("validated OCR text")),
            )?;
        }
        let next_status = if result.vision_required {
            "pending_understanding"
        } else {
            "ready"
        };
        let changed = transaction
            .execute(
                "UPDATE image_assets SET ocr_text = ?1, ocr_confidence = ?2, ocr_engine = ?3, vision_route_reason = ?4, status = ?5, ocr_error_json = NULL, updated_at = ?6 WHERE asset_id = ?7 AND revision_id = ?8 AND status = 'ocr_processing' AND ocr_idempotency_key = ?9",
                params![result.ocr_text, result.confidence, result.engine, result.route_reason, next_status, Utc::now().to_rfc3339(), result.asset_id.to_string(), revision_id.to_string(), result.idempotency_key],
            )
            .map_err(|error| storage_error("IMAGE_OCR_COMMIT_FAILED", error, true))?;
        if changed != 1 {
            return Err(AppError::new(
                "IMAGE_OCR_CHECKPOINT_MISMATCH",
                "图片OCR任务在提交时已经变化",
                true,
            ));
        }
        transaction
            .commit()
            .map_err(|error| storage_error("DATABASE_COMMIT_FAILED", error, true))
    }

    pub fn fail_image_ocr(&self, asset_id: &Uuid, error: &AppError) -> Result<(), AppError> {
        let connection = self.connect()?;
        let attempts = connection
            .query_row(
                "SELECT ocr_attempt_count FROM image_assets WHERE asset_id = ?1",
                [asset_id.to_string()],
                |row| row.get::<_, u32>(0),
            )
            .optional()
            .map_err(|query_error| storage_error("IMAGE_OCR_FAIL_FAILED", query_error, true))?
            .ok_or_else(|| AppError::new("IMAGE_ASSET_NOT_FOUND", "图片OCR任务不存在", false))?;
        let status = if error.retryable && attempts < 2 {
            "pending_ocr"
        } else {
            "pending_understanding"
        };
        connection
            .execute(
                "UPDATE image_assets SET status = ?1, vision_route_reason = ?2, ocr_error_json = ?3, updated_at = ?4 WHERE asset_id = ?5 AND status = 'ocr_processing'",
                params![status, error.code.to_ascii_lowercase(), serde_json::to_string(error).map_err(|serialize_error| AppError::new("IMAGE_OCR_ERROR_INVALID", serialize_error.to_string(), false))?, Utc::now().to_rfc3339(), asset_id.to_string()],
            )
            .map_err(|write_error| storage_error("IMAGE_OCR_FAIL_FAILED", write_error, true))?;
        Ok(())
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
            ocr_attempts: vec![],
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
            "SELECT COUNT(*), SUM(CASE WHEN ia.status = 'ready' THEN 1 ELSE 0 END), SUM(CASE WHEN ia.status IN ('pending_ocr','ocr_processing','pending_understanding','processing') THEN 1 ELSE 0 END) FROM image_assets ia JOIN files f ON f.file_id = ia.file_id WHERE f.current_revision_id = ia.revision_id AND f.availability = 'present' AND {AUTHORIZED_FILE_SQL}"
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
        // SEARCH 链路节点追踪：各通道命中统计（同一搜索的节点共用此
        // correlation_id；operation_id 由线程上下文自动关联）。
        let trace_correlation_id = Uuid::now_v7().to_string();
        let cursor_fingerprint = search_cursor_fingerprint(
            &connection,
            request,
            semantic_query.as_ref().map(|query| query.model_artifact_id),
        )?;
        let offset =
            crate::indexing::decode_search_cursor(request.cursor.as_deref(), &cursor_fingerprint)?;
        let files = list_files_with_connection(&connection)?;
        let query = request.query.trim().to_lowercase();
        let run_filename = matches!(request.mode, SearchMode::Filename | SearchMode::Hybrid);
        let run_fulltext = matches!(request.mode, SearchMode::Fulltext | SearchMode::Hybrid);
        let run_semantic = matches!(request.mode, SearchMode::Hybrid) && semantic_query.is_some();
        let scoped_file_ids = if run_filename || run_semantic {
            Some(collect_scoped_file_ids(
                &connection,
                &files,
                &request.scope,
            )?)
        } else {
            None
        };

        // 三通道并行检索：filename 只遍历内存文件列表，fulltext/semantic 各自
        // 打开独立连接（WAL 下多读并发安全），总耗时从「三路求和」降为
        // 「三路取最大值」，由最慢通道决定。通道是只读查询，失败不影响其他通道。
        let (filename_hits, fulltext_hits, semantic_hits) = std::thread::scope(|scope| {
            let filename_handle = scope.spawn(|| -> Result<Vec<RankedHit>, AppError> {
                let mut hits = Vec::new();
                if run_filename {
                    for file in &files {
                        if let Some(allowed) = scoped_file_ids.as_ref()
                            && !allowed.contains(&file.file_id)
                        {
                            continue;
                        }
                        let name = file.display_name.to_lowercase();
                        let path = file.canonical_path.to_lowercase();
                        // 文件名通道匹配：前缀/规范化/词元覆盖 + 路径包含，
                        // 比原来「整串 contains」覆盖更多真实查询形态（详见 filename_channel_match）。
                        let Some((reason, score)) = filename_channel_match(&name, &path, &query)
                        else {
                            continue;
                        };
                        hits.push(RankedHit {
                            file: file.clone(),
                            chunk_id: None,
                            revision_id: file.current_revision_id,
                            image_asset_id: None,
                            snippet: file.canonical_path.clone(),
                            locator: None,
                            reason,
                            channel_score: score,
                        });
                    }
                    hits.sort_by(|left, right| right.channel_score.total_cmp(&left.channel_score));
                }
                Ok(hits)
            });
            let fulltext_handle = scope.spawn(|| -> Result<Vec<RankedHit>, AppError> {
                if !run_fulltext {
                    return Ok(Vec::new());
                }
                let connection = self.connect()?;
                let mut hits = search_fulltext(&connection, &request.query, &request.scope)?;
                hits.sort_by(|left, right| right.channel_score.total_cmp(&left.channel_score));
                Ok(hits)
            });
            let semantic_handle = scope.spawn(|| -> Result<Vec<RankedHit>, AppError> {
                if run_semantic {
                    let semantic_query =
                        semantic_query.expect("semantic query prepared when run_semantic");
                    let connection = self.connect()?;
                    let scoped = scoped_file_ids
                        .as_ref()
                        .expect("semantic scope is prepared");
                    return search_semantic(&connection, &semantic_query, scoped);
                }
                Ok(Vec::new())
            });
            let filename = join_search_channel(filename_handle)?;
            let fulltext = join_search_channel(fulltext_handle)?;
            let semantic = join_search_channel(semantic_handle)?;
            Ok::<_, AppError>((filename, fulltext, semantic))
        })?;

        let _ = self.record_node_trace(
            "search",
            "filename_search",
            &trace_correlation_id,
            None,
            None,
            &serde_json::json!({
                "query": request.query,
                "run": run_filename,
                "scoped_files": scoped_file_ids.as_ref().map(|set| set.len()).unwrap_or(0),
            }),
            &serde_json::json!({
                "hit_count": filename_hits.len(),
                "top": filename_hits.iter().take(5).map(|hit| serde_json::json!({
                    "file_name": hit.file.display_name,
                    "score": hit.channel_score,
                })).collect::<Vec<_>>(),
            }),
            "ok",
            None,
        );

        let _ = self.record_node_trace(
            "search",
            "fts_search",
            &trace_correlation_id,
            None,
            None,
            &serde_json::json!({ "query": request.query, "run": run_fulltext }),
            &serde_json::json!({
                "hit_count": fulltext_hits.len(),
                "top": fulltext_hits.iter().take(5).map(|hit| serde_json::json!({
                    "file_name": hit.file.display_name,
                    "score": hit.channel_score,
                })).collect::<Vec<_>>(),
            }),
            "ok",
            None,
        );

        let _ = self.record_node_trace(
            "search",
            "semantic_search",
            &trace_correlation_id,
            None,
            None,
            &serde_json::json!({ "query": request.query, "run": run_semantic }),
            &serde_json::json!({
                "hit_count": semantic_hits.len(),
                "top": semantic_hits.iter().take(5).map(|hit| serde_json::json!({
                    "file_name": hit.file.display_name,
                    "score": hit.channel_score,
                })).collect::<Vec<_>>(),
            }),
            "ok",
            None,
        );

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
        let _ = self.record_node_trace(
            "search",
            "fusion_ranking",
            &trace_correlation_id,
            None,
            None,
            &serde_json::json!({
                "query": request.query,
                "mode": format!("{:?}", request.mode),
                "sort": format!("{:?}", request.sort),
                "offset": offset,
                "page_size": page_size,
                "channels_run": {
                    "filename": run_filename,
                    "fulltext": run_fulltext,
                    "semantic": run_semantic,
                },
            }),
            &serde_json::json!({
                "result_count": session.results.len(),
                "has_more": has_more,
                "top": session.results.iter().take(5).map(|hit| serde_json::json!({
                    "file_name": hit.name,
                    "locator": hit.locator,
                    "score": hit.scores.fused,
                })).collect::<Vec<_>>(),
            }),
            "ok",
            None,
        );
        Ok(session)
    }

    pub fn answer_extractively(
        &self,
        request: &AskRequest,
        semantic_query: Option<SemanticQuery<'_>>,
    ) -> Result<AnswerResult, AppError> {
        request.validate()?;
        let started_at = std::time::Instant::now();
        let connection = self.connect()?;
        let files = list_files_with_connection(&connection)?;
        let scoped_file_ids = collect_scoped_file_ids(&connection, &files, &request.scope)?;
        // Questions often identify the intended source explicitly, for example
        // `《季度复盘.md》主要讲了什么？`. Once an exact, authorized filename
        // is present, searching unrelated files only adds semantically plausible
        // but wrong evidence. Narrow both retrieval channels to the exact matches;
        // an unknown title deliberately falls back to the caller's original scope.
        let explicitly_named_file_ids = explicitly_named_file_ids(
            &request.question,
            files
                .iter()
                .map(|file| (file.file_id, file.display_name.as_str())),
            &scoped_file_ids,
        );
        let has_explicit_document_scope = explicitly_named_file_ids.is_some();
        let retrieval_file_ids =
            explicitly_named_file_ids.unwrap_or_else(|| scoped_file_ids.clone());
        let mut fulltext_hits = search_fulltext(&connection, &request.question, &request.scope)?;
        fulltext_hits.retain(|hit| retrieval_file_ids.contains(&hit.file.file_id));
        fulltext_hits.sort_by(|left, right| right.channel_score.total_cmp(&left.channel_score));
        let mut semantic_hits = if let Some(query) = semantic_query.as_ref() {
            search_semantic(&connection, query, &retrieval_file_ids)?
        } else {
            Vec::new()
        };
        semantic_hits.sort_by(|left, right| right.channel_score.total_cmp(&left.channel_score));
        let candidates = crate::indexing::fuse_retrieval_candidates(
            &[fulltext_hits, semantic_hits],
            request.retrieval_limit as usize,
        );
        let session = crate::SearchSession {
            search_id: Uuid::now_v7(),
            status: "completed".into(),
            channels: crate::SearchChannels {
                filename: crate::SearchChannelState::Unavailable,
                fulltext: crate::SearchChannelState::Completed,
                semantic: if semantic_query.is_some() {
                    crate::SearchChannelState::Completed
                } else {
                    crate::SearchChannelState::Unavailable
                },
            },
            results: Vec::new(),
            next_cursor: None,
            elapsed_ms: started_at.elapsed().as_millis() as u64,
        };
        if has_explicit_document_scope && question_requests_document_summary(&request.question) {
            let evidence = load_structural_summary_evidence(
                &connection,
                &files,
                &retrieval_file_ids,
                request.retrieval_limit as usize,
            )?;
            if !evidence.is_empty() {
                let mut answer =
                    crate::assemble_extractive_answer(request, &session, evidence, started_at);
                answer.retrieval_channels = vec!["filename".into(), "document_structure".into()];
                return Ok(answer);
            }
        }
        // 查询级整体门槛：乱码/无关查询的语义 top-1 虚高（模型先验方向）但
        // 无真实命中，整个查询判为与知识库无关 → evidence 为空 → 拒绝回答。
        // 只在语义引擎可用时启用（fulltext-only 回退不受影响）。
        if semantic_query.is_some()
            && !has_explicit_document_scope
            && !crate::indexing::query_has_relevant_evidence(&candidates)
        {
            let mut refused =
                crate::assemble_extractive_answer(request, &session, Vec::new(), started_at);
            refused.no_evidence_reason = Some(crate::ask::NoEvidenceReason::QueryGateRejected);
            return Ok(refused);
        }
        // 融合后有无候选（区分「检索为空」与「有候选但全被相关门槛滤掉」，
        // 供 NO_EVIDENCE 六分类诊断）
        let had_candidates = !candidates.is_empty();
        let mut evidence = Vec::new();
        let mut evidence_tokens = 0_u64;
        for candidate in candidates.into_iter().filter(|candidate| {
            has_explicit_document_scope
                || crate::indexing::candidate_is_relevant_rag_evidence(
                    candidate,
                    semantic_query.is_some(),
                )
        }) {
            let Some(revision_id) = candidate.revision_id else {
                continue;
            };
            let Some(candidate_chunk_id) = candidate.chunk_id else {
                continue;
            };
            let row = connection
                .query_row(
                    "SELECT c.chunk_id, c.node_id, c.text, c.locator_json, n.image_asset_id, c.token_count, c.ordinal FROM chunks c JOIN document_nodes n ON n.node_id = c.node_id JOIN files f ON f.file_id = c.file_id WHERE c.chunk_id = ?1 AND c.file_id = ?2 AND c.revision_id = ?3 AND f.current_revision_id = c.revision_id LIMIT 1",
                    params![candidate_chunk_id.to_string(), candidate.file_id.to_string(), revision_id.to_string()],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, String>(3)?, row.get::<_, Option<String>>(4)?, row.get::<_, u64>(5)?, row.get::<_, i64>(6)?)),
                )
                .optional()
                .map_err(|error| storage_error("ASK_EVIDENCE_QUERY_FAILED", error, true))?;
            let Some((
                chunk_id,
                node_id,
                quote,
                locator_json,
                image_asset_id,
                token_count,
                ordinal,
            )) = row
            else {
                continue;
            };
            // 相邻块上下文：命中块的前/后一块构成线性语境（块间本身已含 64
            // token 重叠），供生成模型理解「这段文字在原文中前后是什么」。
            // 只取同节点的相邻块，跨节点（标题、页眉）不视为相邻。
            let (context_before, context_after) =
                fetch_neighbor_context(&connection, &node_id, ordinal)?;
            if !evidence.is_empty() && evidence_tokens.saturating_add(token_count) > 2_400 {
                continue;
            }
            evidence_tokens = evidence_tokens.saturating_add(token_count);
            let locator = serde_json::from_str::<SourceLocator>(&locator_json)
                .map_err(|error| AppError::new("ASK_EVIDENCE_INVALID", error.to_string(), false))?;
            evidence.push((
                crate::EvidenceRef {
                    evidence_id: Uuid::now_v7(),
                    file_id: candidate.file_id,
                    revision_id,
                    node_id: parse_uuid_value(&node_id)?,
                    chunk_id: parse_uuid_value(&chunk_id)?,
                    image_asset_id: image_asset_id
                        .as_deref()
                        .map(parse_uuid_value)
                        .transpose()?,
                    quote,
                    context_before,
                    context_after,
                    locator,
                    retrieval_score: candidate.scores.fused,
                },
                AnswerSourceFile {
                    file_id: candidate.file_id,
                    display_name: candidate.name,
                    canonical_path: candidate.path,
                },
            ));
        }
        let mut answer = crate::assemble_extractive_answer(request, &session, evidence, started_at);
        answer.retrieval_channels = if semantic_query.is_some() {
            vec!["fts".into(), "embedding".into(), "rrf".into(), "mmr".into()]
        } else {
            vec!["fts".into(), "rrf".into(), "mmr".into()]
        };
        // NO_EVIDENCE 六分类：拒绝路径必须留下根因（有候选但没证据 = 检索层
        // 之外的问题，归 TRUE_NO_EVIDENCE；无候选 = CHUNK_RETRIEVAL_EMPTY）。
        if answer.insufficient_evidence {
            answer.no_evidence_reason = Some(if had_candidates {
                crate::ask::NoEvidenceReason::TrueNoEvidence
            } else {
                crate::ask::NoEvidenceReason::ChunkRetrievalEmpty
            });
        }
        Ok(answer)
    }

    pub fn load_ask_history(
        &self,
        session_id: &Uuid,
        limit: usize,
    ) -> Result<Vec<AskMessage>, AppError> {
        let connection = self.connect()?;
        let mut statement = connection
            .prepare(
                "SELECT message_id, role, content, answer_json, error_json, created_at FROM ask_messages WHERE session_id = ?1 ORDER BY created_at DESC, message_id DESC LIMIT ?2",
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
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, String>(5)?,
                    ))
                },
            )
            .map_err(|error| storage_error("ASK_HISTORY_QUERY_FAILED", error, true))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| storage_error("ASK_HISTORY_QUERY_FAILED", error, true))?;
        let mut messages = rows
            .into_iter()
            .map(
                |(message_id, role, content, answer_json, error_json, created_at)| {
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
                        error: error_json
                            .map(|value| {
                                serde_json::from_str(&value).map_err(|error| {
                                    AppError::new("ASK_HISTORY_INVALID", error.to_string(), false)
                                })
                            })
                            .transpose()?,
                        created_at: parse_datetime_value(&created_at)?,
                    })
                },
            )
            .collect::<Result<Vec<_>, AppError>>()?;
        messages.reverse();
        Ok(messages)
    }

    pub fn list_ask_sessions(
        &self,
        cursor: Option<&str>,
        page_size: u32,
    ) -> Result<AskSessionPage, AppError> {
        if !(1..=100).contains(&page_size) {
            return Err(AppError::new(
                "ASK_SESSION_QUERY_INVALID",
                "问答会话每页数量必须在1到100之间",
                false,
            ));
        }
        let connection = self.connect()?;
        let mut predicates = Vec::new();
        let mut values = Vec::<SqlValue>::new();
        if let Some(encoded) = cursor {
            let cursor: AskSessionKeysetCursor = decode_keyset_cursor(
                encoded,
                "ASK_SESSION_CURSOR_INVALID",
                "问答会话分页游标无效或已过期",
            )?;
            if cursor.version != 1 {
                return Err(AppError::new(
                    "ASK_SESSION_CURSOR_INVALID",
                    "问答会话分页游标版本无效",
                    false,
                ));
            }
            predicates.push("(s.updated_at < ? OR (s.updated_at = ? AND s.session_id < ?))");
            values.push(SqlValue::Text(cursor.updated_at.clone()));
            values.push(SqlValue::Text(cursor.updated_at));
            values.push(SqlValue::Text(cursor.session_id));
        }
        let where_sql = if predicates.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", predicates.join(" AND "))
        };
        values.push(SqlValue::Integer(i64::from(page_size) + 1));
        let sql = format!(
            "SELECT s.session_id, s.title, s.scope_json, s.created_at, s.updated_at, s.last_error_json, (SELECT COUNT(*) FROM ask_messages m WHERE m.session_id = s.session_id) FROM ask_sessions s {where_sql} ORDER BY s.updated_at DESC, s.session_id DESC LIMIT ?"
        );
        let mut statement = connection
            .prepare(&sql)
            .map_err(|error| storage_error("ASK_SESSION_QUERY_FAILED", error, true))?;
        let mut items = statement
            .query_map(params_from_iter(values.iter()), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, u64>(6)?,
                ))
            })
            .map_err(|error| storage_error("ASK_SESSION_QUERY_FAILED", error, true))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| storage_error("ASK_SESSION_QUERY_FAILED", error, true))?
            .into_iter()
            .map(
                |(
                    session_id,
                    title,
                    scope_json,
                    created_at,
                    updated_at,
                    error_json,
                    message_count,
                )| {
                    Ok(AskSessionSummary {
                        session_id: parse_uuid_value(&session_id)?,
                        title: title.unwrap_or_else(|| "未命名会话".into()),
                        scope: serde_json::from_str(&scope_json).map_err(|error| {
                            AppError::new("ASK_SCOPE_INVALID", error.to_string(), false)
                        })?,
                        message_count,
                        created_at: parse_datetime_value(&created_at)?,
                        updated_at: parse_datetime_value(&updated_at)?,
                        last_error: error_json
                            .map(|value| {
                                serde_json::from_str(&value).map_err(|error| {
                                    AppError::new("ASK_HISTORY_INVALID", error.to_string(), false)
                                })
                            })
                            .transpose()?,
                    })
                },
            )
            .collect::<Result<Vec<_>, AppError>>()?;
        let has_more = items.len() > page_size as usize;
        if has_more {
            items.truncate(page_size as usize);
        }
        let next_cursor = if has_more {
            items
                .last()
                .map(|item| AskSessionKeysetCursor {
                    version: 1,
                    updated_at: item.updated_at.to_rfc3339(),
                    session_id: item.session_id.to_string(),
                })
                .map(encode_keyset_cursor)
                .transpose()?
        } else {
            None
        };
        Ok(AskSessionPage {
            items,
            next_cursor,
            has_more,
        })
    }

    pub fn list_ask_messages(
        &self,
        session_id: &Uuid,
        cursor: Option<&str>,
        page_size: u32,
    ) -> Result<AskMessagePage, AppError> {
        if !(1..=200).contains(&page_size) {
            return Err(AppError::new(
                "ASK_MESSAGE_QUERY_INVALID",
                "问答消息每页数量必须在1到200之间",
                false,
            ));
        }
        let connection = self.connect()?;
        let mut predicates = vec!["session_id = ?".to_owned()];
        let mut values = vec![SqlValue::Text(session_id.to_string())];
        if let Some(encoded) = cursor {
            let cursor: AskMessageKeysetCursor = decode_keyset_cursor(
                encoded,
                "ASK_MESSAGE_CURSOR_INVALID",
                "问答消息分页游标无效或已过期",
            )?;
            if cursor.version != 1 {
                return Err(AppError::new(
                    "ASK_MESSAGE_CURSOR_INVALID",
                    "问答消息分页游标版本无效",
                    false,
                ));
            }
            predicates.push("(created_at < ? OR (created_at = ? AND message_id < ?))".into());
            values.push(SqlValue::Text(cursor.created_at.clone()));
            values.push(SqlValue::Text(cursor.created_at));
            values.push(SqlValue::Text(cursor.message_id));
        }
        values.push(SqlValue::Integer(i64::from(page_size) + 1));
        let sql = format!(
            "SELECT message_id, role, content, answer_json, error_json, created_at FROM ask_messages WHERE {} ORDER BY created_at DESC, message_id DESC LIMIT ?",
            predicates.join(" AND ")
        );
        let mut statement = connection
            .prepare(&sql)
            .map_err(|error| storage_error("ASK_HISTORY_QUERY_FAILED", error, true))?;
        let mut items = statement
            .query_map(params_from_iter(values.iter()), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, String>(5)?,
                ))
            })
            .map_err(|error| storage_error("ASK_HISTORY_QUERY_FAILED", error, true))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| storage_error("ASK_HISTORY_QUERY_FAILED", error, true))?
            .into_iter()
            .map(
                |(message_id, role, content, answer_json, error_json, created_at)| {
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
                        error: error_json
                            .map(|value| {
                                serde_json::from_str(&value).map_err(|error| {
                                    AppError::new("ASK_HISTORY_INVALID", error.to_string(), false)
                                })
                            })
                            .transpose()?,
                        created_at: parse_datetime_value(&created_at)?,
                    })
                },
            )
            .collect::<Result<Vec<_>, AppError>>()?;
        let has_more = items.len() > page_size as usize;
        if has_more {
            items.truncate(page_size as usize);
        }
        let next_cursor = if has_more {
            items
                .last()
                .map(|item| AskMessageKeysetCursor {
                    version: 1,
                    created_at: item.created_at.to_rfc3339(),
                    message_id: item.message_id.to_string(),
                })
                .map(encode_keyset_cursor)
                .transpose()?
        } else {
            None
        };
        items.reverse();
        Ok(AskMessagePage {
            items,
            next_cursor,
            has_more,
        })
    }

    pub fn rename_ask_session(&self, session_id: &Uuid, title: &str) -> Result<(), AppError> {
        let title = title.trim();
        if !(1..=80).contains(&title.chars().count()) {
            return Err(AppError::new(
                "ASK_SESSION_TITLE_INVALID",
                "会话名称需要在1到80个字符之间",
                false,
            ));
        }
        let _permit = self.acquire_write(WritePriority::Interactive);
        let connection = self.connect()?;
        let changed = connection
            .execute(
                "UPDATE ask_sessions SET title = ?1, updated_at = ?2 WHERE session_id = ?3",
                params![title, Utc::now().to_rfc3339(), session_id.to_string()],
            )
            .map_err(|error| storage_error("ASK_SESSION_UPDATE_FAILED", error, true))?;
        if changed == 0 {
            return Err(AppError::new(
                "ASK_SESSION_NOT_FOUND",
                "问答会话不存在",
                false,
            ));
        }
        Ok(())
    }

    pub fn delete_ask_session(&self, session_id: &Uuid) -> Result<(), AppError> {
        let _permit = self.acquire_write(WritePriority::Interactive);
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction()
            .map_err(|error| storage_error("ASK_SESSION_DELETE_FAILED", error, true))?;
        // 会话删除必须同步清除四张关联表：ask_messages 虽有外键级联，
        // 但 ask_session_context / node_traces 无外键——若不显式删除，
        // 已删对话的问题、回答与引用原文会永久残留在数据库中。
        transaction
            .execute(
                "DELETE FROM ask_messages WHERE session_id = ?1",
                [session_id.to_string()],
            )
            .map_err(|error| storage_error("ASK_SESSION_DELETE_FAILED", error, true))?;
        transaction
            .execute(
                "DELETE FROM ask_session_context WHERE session_id = ?1",
                [session_id.to_string()],
            )
            .map_err(|error| storage_error("ASK_SESSION_DELETE_FAILED", error, true))?;
        transaction
            .execute(
                "DELETE FROM node_traces WHERE session_id = ?1",
                [session_id.to_string()],
            )
            .map_err(|error| storage_error("ASK_SESSION_DELETE_FAILED", error, true))?;
        let changed = transaction
            .execute(
                "DELETE FROM ask_sessions WHERE session_id = ?1",
                [session_id.to_string()],
            )
            .map_err(|error| storage_error("ASK_SESSION_DELETE_FAILED", error, true))?;
        if changed == 0 {
            return Err(AppError::new(
                "ASK_SESSION_NOT_FOUND",
                "问答会话不存在",
                false,
            ));
        }
        transaction
            .commit()
            .map_err(|error| storage_error("ASK_SESSION_DELETE_FAILED", error, true))
    }

    pub fn record_ask_failure(
        &self,
        request: &AskRequest,
        error: &AppError,
    ) -> Result<(), AppError> {
        let session_id = request
            .session_id
            .ok_or_else(|| AppError::new("ASK_SESSION_INVALID", "失败问答缺少会话标识", false))?;
        let _permit = self.acquire_write(WritePriority::Interactive);
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction()
            .map_err(|db_error| storage_error("ASK_HISTORY_WRITE_FAILED", db_error, true))?;
        let now = Utc::now();
        let scope_json = serde_json::to_string(&request.scope).map_err(|json_error| {
            AppError::new("ASK_SCOPE_INVALID", json_error.to_string(), false)
        })?;
        let error_json = serde_json::to_string(error).map_err(|json_error| {
            AppError::new("ASK_HISTORY_INVALID", json_error.to_string(), false)
        })?;
        transaction
            .execute(
                "INSERT INTO ask_sessions (session_id, scope_json, created_at, updated_at, title, last_error_json) VALUES (?1, ?2, ?3, ?3, ?4, ?5) ON CONFLICT(session_id) DO UPDATE SET scope_json = excluded.scope_json, updated_at = excluded.updated_at, title = COALESCE(ask_sessions.title, excluded.title), last_error_json = excluded.last_error_json",
                params![session_id.to_string(), scope_json, now.to_rfc3339(), ask_session_title(&request.question), error_json],
            )
            .map_err(|db_error| storage_error("ASK_HISTORY_WRITE_FAILED", db_error, true))?;
        transaction
            .execute(
                "INSERT INTO ask_messages (message_id, session_id, role, content, answer_json, error_json, created_at) VALUES (?1, ?2, 'user', ?3, NULL, NULL, ?4)",
                params![Uuid::now_v7().to_string(), session_id.to_string(), request.question.trim(), now.to_rfc3339()],
            )
            .map_err(|db_error| storage_error("ASK_HISTORY_WRITE_FAILED", db_error, true))?;
        transaction
            .execute(
                "INSERT INTO ask_messages (message_id, session_id, role, content, answer_json, error_json, created_at) VALUES (?1, ?2, 'assistant', '', NULL, ?3, ?4)",
                params![Uuid::now_v7().to_string(), session_id.to_string(), error_json, Utc::now().to_rfc3339()],
            )
            .map_err(|db_error| storage_error("ASK_HISTORY_WRITE_FAILED", db_error, true))?;
        transaction
            .commit()
            .map_err(|db_error| storage_error("ASK_HISTORY_WRITE_FAILED", db_error, true))
    }

    pub fn answer_result(&self, message_id: &Uuid) -> Result<AnswerResult, AppError> {
        let connection = self.connect()?;
        let answer_json = connection
            .query_row(
                "SELECT answer_json FROM ask_messages WHERE message_id = ?1 AND role = 'assistant' AND answer_json IS NOT NULL",
                [message_id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| storage_error("ASK_HISTORY_QUERY_FAILED", error, true))?
            .ok_or_else(|| {
                AppError::new(
                    "ASK_RESULT_NOT_FOUND",
                    "要导出的问答结果不存在或尚未完成",
                    false,
                )
            })?;
        serde_json::from_str(&answer_json)
            .map_err(|error| AppError::new("ASK_RESULT_INVALID", error.to_string(), false))
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
        let title = ask_session_title(&request.question);
        transaction
            .execute(
                "INSERT INTO ask_sessions (session_id, scope_json, created_at, updated_at, title, last_error_json) VALUES (?1, ?2, ?3, ?3, ?4, NULL) ON CONFLICT(session_id) DO UPDATE SET scope_json = excluded.scope_json, updated_at = excluded.updated_at, title = COALESCE(ask_sessions.title, excluded.title), last_error_json = NULL",
                params![result.session_id.to_string(), scope_json, now.to_rfc3339(), title],
            )
            .map_err(|error| storage_error("ASK_HISTORY_WRITE_FAILED", error, true))?;
        transaction
            .execute(
                "INSERT INTO ask_messages (message_id, session_id, role, content, answer_json, error_json, created_at) VALUES (?1, ?2, 'user', ?3, NULL, NULL, ?4)",
                params![Uuid::now_v7().to_string(), result.session_id.to_string(), request.question.trim(), now.to_rfc3339()],
            )
            .map_err(|error| storage_error("ASK_HISTORY_WRITE_FAILED", error, true))?;
        let answer_json = serde_json::to_string(result)
            .map_err(|error| AppError::new("ASK_RESULT_INVALID", error.to_string(), false))?;
        transaction
            .execute(
                "INSERT INTO ask_messages (message_id, session_id, role, content, answer_json, error_json, created_at) VALUES (?1, ?2, 'assistant', ?3, ?4, NULL, ?5)",
                params![result.message_id.to_string(), result.session_id.to_string(), result.answer, answer_json, Utc::now().to_rfc3339()],
            )
            .map_err(|error| storage_error("ASK_HISTORY_WRITE_FAILED", error, true))?;
        transaction
            .commit()
            .map_err(|error| storage_error("ASK_HISTORY_WRITE_FAILED", error, true))
    }

    /// 读取会话工作上下文；无记录返回 None（正常路径：新会话）。
    /// Memory 层出错不得阻断问答——读取失败按无上下文处理。
    pub fn get_ask_session_context(
        &self,
        session_id: Uuid,
    ) -> Result<Option<AskSessionContext>, AppError> {
        let connection = self.connect()?;
        let row = connection.query_row(
            "SELECT active_file_id, active_file_ids_json, active_document_type, \
                        active_entity_id, active_collection_id, last_referenced_file_ids_json, \
                        last_intent, updated_at, pending_clarification_reference \
                 FROM ask_session_context WHERE session_id = ?1",
            params![session_id.to_string()],
            |row| {
                let active_file_ids_json: String = row.get(1)?;
                let last_referenced_json: String = row.get(5)?;
                Ok(AskSessionContext {
                    session_id: Some(session_id),
                    active_file_id: row
                        .get::<_, Option<String>>(0)?
                        .as_deref()
                        .and_then(|value| Uuid::parse_str(value).ok()),
                    active_file_ids: serde_json::from_str(&active_file_ids_json)
                        .unwrap_or_default(),
                    active_document_type: row
                        .get::<_, Option<String>>(2)?
                        .as_deref()
                        .and_then(DocumentType::parse_lenient),
                    active_entity_id: row
                        .get::<_, Option<String>>(3)?
                        .as_deref()
                        .and_then(|value| Uuid::parse_str(value).ok()),
                    active_collection_id: row
                        .get::<_, Option<String>>(4)?
                        .as_deref()
                        .and_then(|value| Uuid::parse_str(value).ok()),
                    last_referenced_file_ids: serde_json::from_str(&last_referenced_json)
                        .unwrap_or_default(),
                    last_intent: row.get(6)?,
                    updated_at: row
                        .get::<_, Option<String>>(7)?
                        .as_deref()
                        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
                        .map(|value| value.with_timezone(&Utc)),
                    pending_clarification_reference: row.get(8)?,
                })
            },
        );
        match row {
            Ok(context) => Ok(Some(context)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(storage_error("ASK_CONTEXT_READ_FAILED", error, true)),
        }
    }

    /// 更新会话工作上下文（upsert）。不存在的会话也允许写入（Ask 会在
    /// 落库 exchange 时补建会话行，context 与之解耦）。
    pub fn update_ask_session_context(
        &self,
        session_id: Uuid,
        context: &AskSessionContext,
    ) -> Result<(), AppError> {
        let connection = self.connect()?;
        let active_file_ids_json = serde_json::to_string(&context.active_file_ids)
            .map_err(|error| AppError::new("ASK_CONTEXT_INVALID", error.to_string(), false))?;
        let last_referenced_json = serde_json::to_string(&context.last_referenced_file_ids)
            .map_err(|error| AppError::new("ASK_CONTEXT_INVALID", error.to_string(), false))?;
        let now = context.updated_at.unwrap_or_else(Utc::now);
        connection
            .execute(
                "INSERT INTO ask_session_context \
                    (session_id, active_file_id, active_file_ids_json, active_document_type, \
                     active_entity_id, active_collection_id, last_referenced_file_ids_json, \
                     last_intent, pending_clarification_reference, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) \
                 ON CONFLICT(session_id) DO UPDATE SET \
                    active_file_id = excluded.active_file_id, \
                    active_file_ids_json = excluded.active_file_ids_json, \
                    active_document_type = excluded.active_document_type, \
                    active_entity_id = excluded.active_entity_id, \
                    active_collection_id = excluded.active_collection_id, \
                    last_referenced_file_ids_json = excluded.last_referenced_file_ids_json, \
                    last_intent = excluded.last_intent, \
                    pending_clarification_reference = excluded.pending_clarification_reference, \
                    updated_at = excluded.updated_at",
                params![
                    session_id.to_string(),
                    context.active_file_id.map(|value| value.to_string()),
                    active_file_ids_json,
                    context.active_document_type.map(|value| value.as_str()),
                    context.active_entity_id.map(|value| value.to_string()),
                    context.active_collection_id.map(|value| value.to_string()),
                    last_referenced_json,
                    context.last_intent,
                    context.pending_clarification_reference,
                    now.to_rfc3339(),
                ],
            )
            .map_err(|error| storage_error("ASK_CONTEXT_WRITE_FAILED", error, true))?;
        Ok(())
    }

    /// 清空会话工作上下文（新会话开始 / 会话删除时调用）。
    pub fn clear_ask_session_context(&self, session_id: Uuid) -> Result<(), AppError> {
        let connection = self.connect()?;
        connection
            .execute(
                "DELETE FROM ask_session_context WHERE session_id = ?1",
                params![session_id.to_string()],
            )
            .map_err(|error| storage_error("ASK_CONTEXT_WRITE_FAILED", error, true))?;
        Ok(())
    }

    // ---- Document Profile（document_profiles 扩展列读写）----

    /// 读取文档画像；无画像返回 None。
    /// DocumentProfile 生产链：为「已解析 + 当前 revision 全量嵌入完成」的
    /// 文件构建/重建文档画像。
    ///
    /// 画像内容（纯确定性逻辑，无 LLM）：
    /// - title = files.display_name；
    /// - section_titles = 从 document_nodes.heading_path_json 提取的叶子标题；
    /// - summary = 首个代表性 chunk 的压缩文本；
    /// - 代表性文本 = title + section_titles + head/mid/tail chunk，取其
    ///   sha256 作为 representative_text_hash；
    /// - 文档级向量 = 文件前 3 个 chunk 嵌入的均值（与集合建议口径一致）。
    ///
    /// 生命周期：画像绑定构建时的 revision_id；stale 画像（revision 不匹配）
    /// 在 list_document_profiles 检索侧被过滤，绝不用于定位新 revision。
    /// 单文件构建失败只跳过该文件（skipped_files+1），不影响其他文件；
    /// 构建链错误只降级 Document Resolver 的定位能力。
    pub fn refresh_document_profiles(
        &self,
        model_artifact_id: &str,
        max_files: u32,
    ) -> Result<ProfileRefreshResult, AppError> {
        if max_files == 0 || max_files > 2000 {
            return Err(AppError::new(
                "PROFILE_REFRESH_INVALID",
                "画像构建需要有效的 Embedding 模型，且单批文件数必须在 1 到 2000 之间",
                false,
            ));
        }
        const PROFILE_ALGORITHM_VERSION: &str = "profile_base_v1";
        let connection = self.connect()?;
        // 候选：parse 完成 + availability=present + 画像缺失/过期（revision 或
        // Embedding 模型变化、无章节标题、无代表性文本哈希）+ 当前 revision
        // 的 chunk 已**全量**嵌入（部分嵌入不建画像，等待嵌入收敛后再构建）。
        // 与 refresh_collection_suggestions 共用「画像缺失或过期」语义，但
        // 不依赖 algorithm_version（集合聚类的算法标签与本链无关）。
        let targets_sql = format!(
            "WITH targets AS MATERIALIZED (
                SELECT f.file_id, f.current_revision_id, f.display_name
                FROM files f
                LEFT JOIN document_profiles p ON p.file_id = f.file_id
                WHERE f.current_revision_id IS NOT NULL
                  AND f.parse_status = 'parsed'
                  AND f.availability = 'present'
                  AND {AUTHORIZED_FILE_SQL}
                  AND EXISTS (SELECT 1 FROM chunk_embeddings ce
                              WHERE ce.file_id = f.file_id AND ce.revision_id = f.current_revision_id
                                AND ce.model_artifact_id = ?1)
                  AND NOT EXISTS (
                        SELECT 1 FROM chunks c2
                        WHERE c2.file_id = f.file_id AND c2.revision_id = f.current_revision_id
                          AND NOT EXISTS (
                                SELECT 1 FROM chunk_embeddings ce2
                                WHERE ce2.chunk_id = c2.chunk_id AND ce2.model_artifact_id = ?1))
                  AND (p.file_id IS NULL OR p.revision_id <> f.current_revision_id
                       OR p.embedding_model_id <> ?1 OR p.section_titles_json = '[]'
                       OR p.representative_text_hash IS NULL)
                ORDER BY f.last_seen_at DESC LIMIT ?2
            ) SELECT t.file_id, t.current_revision_id, t.display_name FROM targets t"
        );
        let targets = {
            let mut statement = connection
                .prepare(&targets_sql)
                .map_err(|error| storage_error("PROFILE_REFRESH_QUERY_FAILED", error, true))?;
            statement
                .query_map(params![model_artifact_id, i64::from(max_files)], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })
                .map_err(|error| storage_error("PROFILE_REFRESH_QUERY_FAILED", error, true))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| storage_error("PROFILE_REFRESH_QUERY_FAILED", error, true))?
        };
        // 代表性 chunk：头（rn=1）/ 中（rn=ceil(total/2)）/ 尾（rn=total），
        // 一条窗口查询取回，不整表读入。
        const REPRESENTATIVE_CHUNKS_SQL: &str = "SELECT text FROM (SELECT c.text, ROW_NUMBER() OVER (ORDER BY c.ordinal) AS rn, \
             COUNT(*) OVER () AS total FROM chunks c WHERE c.file_id = ?1 AND c.revision_id = ?2) \
             WHERE rn = 1 OR rn = total OR rn = (total + 1) / 2";
        // 章节标题：全部节点的 heading_path_json（确定性提取在 profile_builder）。
        const SECTION_TITLES_SQL: &str = "SELECT heading_path_json FROM document_nodes dn WHERE dn.revision_id = ?1 ORDER BY dn.ordinal";
        let mut vector_cache = HashMap::<Uuid, Vec<f32>>::new();
        type ProfileRefreshRecord = (
            String,
            String,
            String,
            String,
            String,
            u32,
            Vec<u8>,
            String,
            String,
            String,
        );
        let mut records: Vec<ProfileRefreshRecord> = Vec::new();
        let mut skipped_files: u64 = 0;
        for (file_id, revision_id, title) in targets {
            let file_id = match Uuid::parse_str(&file_id) {
                Ok(value) => value,
                Err(_) => {
                    skipped_files += 1;
                    continue;
                }
            };
            let chunks = {
                let mut statement = connection
                    .prepare(REPRESENTATIVE_CHUNKS_SQL)
                    .map_err(|error| storage_error("PROFILE_REFRESH_QUERY_FAILED", error, true))?;
                statement
                    .query_map(params![file_id.to_string(), revision_id], |row| {
                        row.get::<_, String>(0)
                    })
                    .map_err(|error| storage_error("PROFILE_REFRESH_QUERY_FAILED", error, true))?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|error| storage_error("PROFILE_REFRESH_QUERY_FAILED", error, true))?
            };
            let (head, mid, tail) = pick_head_mid_tail(&chunks);
            let section_titles = {
                let mut statement = connection
                    .prepare(SECTION_TITLES_SQL)
                    .map_err(|error| storage_error("PROFILE_REFRESH_QUERY_FAILED", error, true))?;
                let rows = statement
                    .query_map(params![revision_id], |row| row.get::<_, String>(0))
                    .map_err(|error| storage_error("PROFILE_REFRESH_QUERY_FAILED", error, true))?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|error| storage_error("PROFILE_REFRESH_QUERY_FAILED", error, true))?;
                extract_section_titles(
                    rows.iter()
                        .enumerate()
                        .map(|(index, json)| (index as u64, json.as_str())),
                )
            };
            // 文档级向量缺失 → 跳过（本次不建画像，等嵌入收敛后下轮重建）。
            let Some(vector) = file_vector_for(
                &connection,
                Some(model_artifact_id),
                file_id,
                &mut vector_cache,
            )?
            else {
                skipped_files += 1;
                continue;
            };
            let representative =
                build_representative_text(&title, &section_titles, &head, &mid, &tail);
            let summary = compact_profile_text(&head, 260);
            let keywords = profile_keywords(&title);
            records.push((
                file_id.to_string(),
                revision_id,
                title,
                summary,
                serde_json::to_string(&keywords).unwrap_or_else(|_| "[]".into()),
                vector.len() as u32,
                encode_vector(&vector),
                semantic_bucket(&vector),
                serde_json::to_string(&section_titles).unwrap_or_else(|_| "[]".into()),
                representative_text_hash(&representative),
            ));
        }
        drop(connection);
        let mut profiled_files = 0_u64;
        for batch in records.chunks(100) {
            let _permit = self.acquire_write(WritePriority::Background);
            let mut connection = self.connect()?;
            let transaction = connection
                .transaction()
                .map_err(|error| storage_error("PROFILE_REFRESH_WRITE_FAILED", error, true))?;
            for (
                file_id,
                revision_id,
                title,
                summary,
                keywords,
                dimension,
                vector,
                bucket,
                section_titles,
                hash,
            ) in batch
            {
                let now = Utc::now().to_rfc3339();
                // 只更新基础列 + 章节/哈希列；document_type/type_confidence 归
                // 分类器所有（Step 2），本链绝不覆写。但 revision 变化时分类器
                // 结果属于旧内容，必须清空等待重新分类（旧类型绝不带到新版本）。
                transaction
                    .execute(
                        "INSERT INTO document_profiles (file_id, revision_id, title, summary, keywords_json, entities_json, embedding_model_id, dimension, vector_blob, candidate_bucket, algorithm_version, section_titles_json, representative_text_hash, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, '[]', ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?13) ON CONFLICT(file_id) DO UPDATE SET revision_id = excluded.revision_id, title = excluded.title, summary = excluded.summary, keywords_json = excluded.keywords_json, entities_json = excluded.entities_json, embedding_model_id = excluded.embedding_model_id, dimension = excluded.dimension, vector_blob = excluded.vector_blob, candidate_bucket = excluded.candidate_bucket, algorithm_version = excluded.algorithm_version, section_titles_json = excluded.section_titles_json, representative_text_hash = excluded.representative_text_hash, document_type = CASE WHEN document_profiles.revision_id <> excluded.revision_id THEN NULL ELSE document_profiles.document_type END, type_confidence = CASE WHEN document_profiles.revision_id <> excluded.revision_id THEN NULL ELSE document_profiles.type_confidence END, updated_at = excluded.updated_at",
                        params![
                            file_id,
                            revision_id,
                            title,
                            summary,
                            keywords,
                            model_artifact_id,
                            dimension,
                            vector,
                            bucket,
                            PROFILE_ALGORITHM_VERSION,
                            section_titles,
                            hash,
                            now,
                        ],
                    )
                    .map_err(|error| storage_error("PROFILE_REFRESH_WRITE_FAILED", error, true))?;
                profiled_files = profiled_files.saturating_add(1);
            }
            transaction
                .commit()
                .map_err(|error| storage_error("PROFILE_REFRESH_WRITE_FAILED", error, true))?;
        }
        Ok(ProfileRefreshResult {
            profiled_files,
            skipped_files,
        })
    }

    /// 强制重建文档画像（单文件或全部）。**不重新生成 Chunk Embedding**——
    /// 画像构建仍要求当前 revision 的 chunk 已全量嵌入（部分嵌入的文件跳过）。
    ///
    /// 实现：先删除目标画像行（沿用 revision 变更语义：document_type /
    /// type_confidence 一并清空，旧分类结果绝不带到重建结果），再走常规
    /// refresh 路径重建；全部模式按批循环直到没有缺失画像（不可建文件
    /// 永远不入选 targets，不会死循环）。
    pub fn rebuild_document_profiles(
        &self,
        model_artifact_id: &str,
        file_ids: Option<&[Uuid]>,
    ) -> Result<ProfileRefreshResult, AppError> {
        let _permit = self.acquire_write(WritePriority::Background);
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction()
            .map_err(|error| storage_error("PROFILE_REBUILD_WRITE_FAILED", error, true))?;
        match file_ids {
            Some(ids) => {
                for file_id in ids {
                    transaction
                        .execute(
                            "DELETE FROM document_profiles WHERE file_id = ?1",
                            params![file_id.to_string()],
                        )
                        .map_err(|error| {
                            storage_error("PROFILE_REBUILD_WRITE_FAILED", error, true)
                        })?;
                }
            }
            None => {
                transaction
                    .execute("DELETE FROM document_profiles", [])
                    .map_err(|error| storage_error("PROFILE_REBUILD_WRITE_FAILED", error, true))?;
            }
        }
        transaction
            .commit()
            .map_err(|error| storage_error("PROFILE_REBUILD_WRITE_FAILED", error, true))?;
        drop(connection);
        let mut profiled_files = 0_u64;
        let mut skipped_files = 0_u64;
        let mut batch = self.refresh_document_profiles(model_artifact_id, 2000)?;
        profiled_files = profiled_files.saturating_add(batch.profiled_files);
        skipped_files = skipped_files.saturating_add(batch.skipped_files);
        if file_ids.is_none() {
            while batch.profiled_files > 0 {
                batch = self.refresh_document_profiles(model_artifact_id, 2000)?;
                profiled_files = profiled_files.saturating_add(batch.profiled_files);
                skipped_files = skipped_files.saturating_add(batch.skipped_files);
            }
        }
        Ok(ProfileRefreshResult {
            profiled_files,
            skipped_files,
        })
    }

    pub fn get_document_profile(&self, file_id: Uuid) -> Result<Option<DocumentProfile>, AppError> {
        let connection = self.connect()?;
        let row = connection.query_row(
            "SELECT p.file_id, p.revision_id, p.title, p.summary, p.keywords_json, \
                        p.entities_json, p.document_type, p.type_confidence, \
                        p.section_titles_json, p.representative_text_hash, p.updated_at \
                 FROM document_profiles p WHERE p.file_id = ?1",
            params![file_id.to_string()],
            map_profile_row,
        );
        match row {
            Ok((raw, _)) => Ok(Some(parse_profile_row(raw)?)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(storage_error("DOCUMENT_PROFILE_READ_FAILED", error, true)),
        }
    }

    /// 写入画像的分类器扩展列（document_type/type_confidence/section_titles/
    /// representative_text_hash）。只更新扩展列，不动 organizing.rs 拥有的
    /// 其余列；画像行尚不存在时返回 Ok(false)（分类器在画像就绪后运行）。
    pub fn update_document_profile_classifier(
        &self,
        profile: &DocumentProfile,
    ) -> Result<bool, AppError> {
        let connection = self.connect()?;
        let section_titles_json = serde_json::to_string(&profile.section_titles)
            .map_err(|error| AppError::new("DOCUMENT_PROFILE_INVALID", error.to_string(), false))?;
        let changed = connection
            .execute(
                "UPDATE document_profiles SET \
                    document_type = ?2, \
                    type_confidence = ?3, \
                    section_titles_json = ?4, \
                    representative_text_hash = ?5, \
                    updated_at = ?6 \
                 WHERE file_id = ?1",
                params![
                    profile.file_id.to_string(),
                    profile.document_type.map(|value| value.as_str()),
                    profile.type_confidence.map(f64::from),
                    section_titles_json,
                    profile.representative_text_hash,
                    profile.updated_at.to_rfc3339(),
                ],
            )
            .map_err(|error| storage_error("DOCUMENT_PROFILE_WRITE_FAILED", error, true))?;
        Ok(changed > 0)
    }

    /// 列出文档画像（可按文档类型过滤），返回 (画像, 文件名)。
    /// 供 Document Resolver 定位目标文件；limit 保护画像库过大。
    ///
    /// 生命周期失效（Step 1）：只返回 `revision_id = f.current_revision_id`
    /// 的**当前**画像——stale 画像（文件更新后尚未重建）绝不参与定位，
    /// 避免把「旧版本的简历」解析成用户现在指的「简历」。
    /// 当前修订的 document_nodes 总数（SUMMARY 节点上限截断的 trace 用；
    /// 计数当前修订，文件不存在/未解析 → 0）。
    pub fn file_document_node_count(&self, file_id: &Uuid) -> Result<u64, AppError> {
        let connection = self.connect()?;
        let count: u64 = connection
            .query_row(
                "SELECT COUNT(*) FROM document_nodes n \
                 JOIN files f ON f.file_id = ?1 AND n.revision_id = f.current_revision_id",
                params![file_id.to_string()],
                |row| row.get(0),
            )
            .map_err(|error| storage_error("DOCUMENT_NODE_COUNT_FAILED", error, true))?;
        Ok(count)
    }

    pub fn list_document_profiles(
        &self,
        document_type: Option<DocumentType>,
        limit: u32,
    ) -> Result<Vec<(DocumentProfile, String)>, AppError> {
        let connection = self.connect()?;
        let type_clause = if document_type.is_some() {
            "AND p.document_type = ?2"
        } else {
            ""
        };
        let sql = format!(
            "SELECT p.file_id, p.revision_id, p.title, p.summary, p.keywords_json, \
                    p.entities_json, p.document_type, p.type_confidence, \
                    p.section_titles_json, p.representative_text_hash, p.updated_at, f.name \
             FROM document_profiles p JOIN files f ON f.file_id = p.file_id \
             WHERE f.availability = 'present' AND p.revision_id = f.current_revision_id {type_clause} \
             ORDER BY p.updated_at DESC LIMIT ?1"
        );
        let mut statement = connection
            .prepare(&sql)
            .map_err(|error| storage_error("DOCUMENT_PROFILE_LIST_FAILED", error, true))?;
        let mut rows = if let Some(document_type) = document_type {
            statement
                .query_map(
                    params![limit as i64, document_type.as_str()],
                    map_profile_row,
                )
                .map_err(|error| storage_error("DOCUMENT_PROFILE_LIST_FAILED", error, true))?
        } else {
            statement
                .query_map(params![limit as i64], map_profile_row)
                .map_err(|error| storage_error("DOCUMENT_PROFILE_LIST_FAILED", error, true))?
        };
        let mut profiles = Vec::new();
        for row in rows.by_ref() {
            let (raw, name) =
                row.map_err(|error| storage_error("DOCUMENT_PROFILE_LIST_FAILED", error, true))?;
            // 单行损坏跳过（定位是 best-effort，不因一行坏画像拖垮整次解析）
            if let Ok(profile) = parse_profile_row(raw) {
                profiles.push((profile, name.unwrap_or_default()));
            }
        }
        drop(rows);
        Ok(profiles)
    }

    /// 列出待分类画像：`document_type IS NULL` 且当前（revision 匹配、文件在场）。
    /// 分类器（Step 2）在画像就绪后扫描这些行；revision 变更时画像的
    /// document_type 已被重建链清空，因此这里的行即「新内容尚未分类」。
    /// 按 updated_at DESC：最后更新的画像优先（用户最可能马上问到）。
    pub fn list_profiles_needing_classification(
        &self,
        limit: u32,
    ) -> Result<Vec<(DocumentProfile, String)>, AppError> {
        let connection = self.connect()?;
        let mut statement = connection
            .prepare(
                "SELECT p.file_id, p.revision_id, p.title, p.summary, p.keywords_json, \
                        p.entities_json, p.document_type, p.type_confidence, \
                        p.section_titles_json, p.representative_text_hash, p.updated_at, f.name \
                 FROM document_profiles p JOIN files f ON f.file_id = p.file_id \
                 WHERE f.availability = 'present' AND p.revision_id = f.current_revision_id \
                   AND p.document_type IS NULL \
                 ORDER BY p.updated_at DESC LIMIT ?1",
            )
            .map_err(|error| storage_error("DOCUMENT_PROFILE_LIST_FAILED", error, true))?;
        let mut rows = statement
            .query_map(params![limit as i64], map_profile_row)
            .map_err(|error| storage_error("DOCUMENT_PROFILE_LIST_FAILED", error, true))?;
        let mut profiles = Vec::new();
        for row in rows.by_ref() {
            let (raw, name) =
                row.map_err(|error| storage_error("DOCUMENT_PROFILE_LIST_FAILED", error, true))?;
            // 单行损坏跳过（分类是 best-effort，不因一行坏画像拖垮整批）
            if let Ok(profile) = parse_profile_row(raw) {
                profiles.push((profile, name.unwrap_or_default()));
            }
        }
        drop(rows);
        Ok(profiles)
    }

    /// 读取画像已存的文档级向量（分类器与原型向量比对用）。
    /// 画像存在但向量缺失/损坏时返回 None——分类退化为纯规则路径，不阻塞。
    pub fn profile_vector(&self, file_id: &Uuid) -> Result<Option<Vec<f32>>, AppError> {
        let connection = self.connect()?;
        let row = connection.query_row(
            "SELECT vector_blob, dimension FROM document_profiles WHERE file_id = ?1",
            params![file_id.to_string()],
            |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, u32>(1)?)),
        );
        match row {
            Ok((blob, dimension)) => Ok(decode_vector(&blob, dimension)
                .ok()
                .filter(|vector| !vector.is_empty())),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(storage_error("DOCUMENT_PROFILE_READ_FAILED", error, true)),
        }
    }

    /// 批量读取画像向量（文档级召回：metadata 预筛后只取候选集的向量，
    /// 避免对全库逐文件查一次库）。缺失/损坏的向量静默跳过——召回是
    /// 增益层，不因单条坏向量拖垮整批。
    pub fn profile_vectors(
        &self,
        file_ids: &[Uuid],
    ) -> Result<std::collections::HashMap<Uuid, Vec<f32>>, AppError> {
        let mut vectors = std::collections::HashMap::new();
        if file_ids.is_empty() {
            return Ok(vectors);
        }
        let placeholders = std::iter::repeat_n("?", file_ids.len())
            .collect::<Vec<_>>()
            .join(",");
        let connection = self.connect()?;
        let sql = format!(
            "SELECT file_id, vector_blob, dimension FROM document_profiles \
             WHERE file_id IN ({placeholders})"
        );
        let mut statement = connection
            .prepare(&sql)
            .map_err(|error| storage_error("DOCUMENT_PROFILE_VECTORS_FAILED", error, true))?;
        let params: Vec<String> = file_ids.iter().map(|id| id.to_string()).collect();
        let mut rows = statement
            .query_map(rusqlite::params_from_iter(params.iter()), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, u32>(2)?,
                ))
            })
            .map_err(|error| storage_error("DOCUMENT_PROFILE_VECTORS_FAILED", error, true))?;
        for row in rows.by_ref() {
            match row {
                Ok((file_id, blob, dimension)) => {
                    if let Ok(id) = uuid::Uuid::parse_str(&file_id)
                        && let Ok(vector) = decode_vector(&blob, dimension)
                        && !vector.is_empty()
                    {
                        vectors.insert(id, vector);
                    }
                }
                Err(_) => continue,
            }
        }
        drop(rows);
        Ok(vectors)
    }

    // ============================== Memory 数据层（Step 3） ==============================
    //
    // Memory 不是第二份知识库：只存「用户 ↔ 实体 ↔ 文件」的关系、别名和稳定
    // 信息，帮助理解和定位。不复制 Chunk 正文，绝不能作为最终事实证据。
    // 合并规则：同一三元组/别名重复写入时，低信任来源不能覆盖高信任来源
    // （rank 见 MemorySource）；推断类来源只允许 candidate。

    /// 插入或取回实体（按 (entity_type, canonical_name) 去重）。
    pub fn upsert_memory_entity(
        &self,
        entity_type: &str,
        canonical_name: &str,
        metadata_json: &serde_json::Value,
    ) -> Result<Uuid, AppError> {
        let connection = self.connect()?;
        let now = Utc::now().to_rfc3339();
        connection
            .execute(
                "INSERT INTO memory_entities (entity_id, entity_type, canonical_name, metadata_json, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?5) ON CONFLICT(entity_type, canonical_name) DO NOTHING",
                params![Uuid::now_v7().to_string(), entity_type, canonical_name, metadata_json.to_string(), now],
            )
            .map_err(|error| storage_error("MEMORY_ENTITY_WRITE_FAILED", error, true))?;
        let entity_id: String = connection
            .query_row(
                "SELECT entity_id FROM memory_entities WHERE entity_type = ?1 AND canonical_name = ?2",
                params![entity_type, canonical_name],
                |row| row.get(0),
            )
            .map_err(|error| storage_error("MEMORY_ENTITY_READ_FAILED", error, true))?;
        Uuid::parse_str(&entity_id)
            .map_err(|error| AppError::new("MEMORY_ENTITY_INVALID", error.to_string(), false))
    }

    pub fn memory_entity_by_id(&self, entity_id: Uuid) -> Result<Option<MemoryEntity>, AppError> {
        let connection = self.connect()?;
        let row = connection
            .query_row(
                "SELECT entity_id, entity_type, canonical_name, metadata_json, created_at, updated_at FROM memory_entities WHERE entity_id = ?1",
                params![entity_id.to_string()],
                map_memory_entity_row,
            );
        match row {
            Ok(entity) => Ok(Some(entity)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(storage_error("MEMORY_ENTITY_READ_FAILED", error, true)),
        }
    }

    pub fn memory_entity_by_name(
        &self,
        entity_type: &str,
        canonical_name: &str,
    ) -> Result<Option<MemoryEntity>, AppError> {
        let connection = self.connect()?;
        let row = connection
            .query_row(
                "SELECT entity_id, entity_type, canonical_name, metadata_json, created_at, updated_at FROM memory_entities WHERE entity_type = ?1 AND canonical_name = ?2",
                params![entity_type, canonical_name],
                map_memory_entity_row,
            );
        match row {
            Ok(entity) => Ok(Some(entity)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(storage_error("MEMORY_ENTITY_READ_FAILED", error, true)),
        }
    }

    pub fn list_memory_entities(&self, limit: u32) -> Result<Vec<MemoryEntity>, AppError> {
        let connection = self.connect()?;
        let mut statement = connection
            .prepare(
                "SELECT entity_id, entity_type, canonical_name, metadata_json, created_at, updated_at FROM memory_entities ORDER BY updated_at DESC LIMIT ?1",
            )
            .map_err(|error| storage_error("MEMORY_ENTITY_READ_FAILED", error, true))?;
        let rows = statement
            .query_map(params![limit as i64], map_memory_entity_row)
            .map_err(|error| storage_error("MEMORY_ENTITY_READ_FAILED", error, true))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| storage_error("MEMORY_ENTITY_READ_FAILED", error, true))?;
        drop(statement);
        Ok(rows)
    }

    /// 修改实体（名称/类型变更时迁移同实体关系？不：名称变更会破坏别名/关系
    /// 的语义绑定，因此只允许修改元数据与规范化展示名，关系仍按 entity_id 指向）。
    pub fn update_memory_entity(
        &self,
        entity_id: Uuid,
        canonical_name: &str,
        metadata_json: &serde_json::Value,
    ) -> Result<bool, AppError> {
        let connection = self.connect()?;
        let changed = connection
            .execute(
                "UPDATE memory_entities SET canonical_name = ?2, metadata_json = ?3, updated_at = ?4 WHERE entity_id = ?1",
                params![entity_id.to_string(), canonical_name, metadata_json.to_string(), Utc::now().to_rfc3339()],
            )
            .map_err(|error| storage_error("MEMORY_ENTITY_WRITE_FAILED", error, true))?;
        Ok(changed > 0)
    }

    /// 删除实体，并级联清理引用它的关系（subject/object）与别名（target）。
    pub fn delete_memory_entity(&self, entity_id: Uuid) -> Result<bool, AppError> {
        let connection = self.connect()?;
        let _permit = self.acquire_write(WritePriority::Background);
        let transaction = connection
            .unchecked_transaction()
            .map_err(|error| storage_error("MEMORY_ENTITY_DELETE_FAILED", error, true))?;
        transaction
            .execute(
                "DELETE FROM memory_relations WHERE (subject_type = 'entity' AND subject_id = ?1) OR (object_type = 'entity' AND object_id = ?1)",
                params![entity_id.to_string()],
            )
            .map_err(|error| storage_error("MEMORY_ENTITY_DELETE_FAILED", error, true))?;
        transaction
            .execute(
                "DELETE FROM memory_aliases WHERE target_type = 'entity' AND target_id = ?1",
                params![entity_id.to_string()],
            )
            .map_err(|error| storage_error("MEMORY_ENTITY_DELETE_FAILED", error, true))?;
        let changed = transaction
            .execute(
                "DELETE FROM memory_entities WHERE entity_id = ?1",
                params![entity_id.to_string()],
            )
            .map_err(|error| storage_error("MEMORY_ENTITY_DELETE_FAILED", error, true))?;
        transaction
            .commit()
            .map_err(|error| storage_error("MEMORY_ENTITY_DELETE_FAILED", error, true))?;
        Ok(changed > 0)
    }

    /// 写入（或按来源等级合并）一条关系。
    ///
    /// 合并规则（确定性）：
    /// - 新三元组 → 插入，status 按输入（推断类来源强制降为 candidate）；
    /// - 已存在：输入来源等级 >= 现有等级 → 覆盖 status/confidence/source；
    ///   等级更低 → 原样保留（低信任不能覆盖高信任事实，也不能复活 stale/rejected）。
    pub fn upsert_memory_relation(&self, input: &MemoryWriteInput) -> Result<Uuid, AppError> {
        let connection = self.connect()?;
        let now = Utc::now().to_rfc3339();
        let existing: Option<(String, MemorySource, MemoryStatus)> = connection
            .query_row(
                "SELECT relation_id, source_type, status FROM memory_relations WHERE subject_type = ?1 AND subject_id = ?2 AND predicate = ?3 AND object_type = ?4 AND object_id = ?5",
                params![
                    input.subject_type.as_storage(),
                    input.subject_id.to_string(),
                    input.predicate,
                    input.object_type.as_storage(),
                    input.object_id.to_string(),
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        MemorySource::parse_storage(&row.get::<_, String>(1)?),
                        MemoryStatus::parse_storage(&row.get::<_, String>(2)?),
                    ))
                },
            )
            .optional()
            .map_err(|error| storage_error("MEMORY_RELATION_READ_FAILED", error, true))?;
        let existing_rank = existing
            .as_ref()
            .map(|(_, source, _)| source.rank())
            .unwrap_or(0);
        let status =
            if input.status == MemoryStatus::Confirmed && !input.source_type.allows_confirmed() {
                MemoryStatus::Candidate
            } else {
                input.status
            };
        if let Some((relation_id, _, _)) = existing {
            if input.source_type.rank() >= existing_rank {
                connection
                    .execute(
                        "UPDATE memory_relations SET confidence = ?2, status = ?3, source_type = ?4, source_id = ?5, updated_at = ?6 WHERE relation_id = ?1",
                        params![
                            relation_id,
                            f64::from(input.confidence.clamp(0.0, 1.0)),
                            status.as_storage(),
                            input.source_type.as_storage(),
                            input.source_id,
                            now,
                        ],
                    )
                    .map_err(|error| storage_error("MEMORY_RELATION_WRITE_FAILED", error, true))?;
            }
            return Uuid::parse_str(&relation_id).map_err(|error| {
                AppError::new("MEMORY_RELATION_INVALID", error.to_string(), false)
            });
        }
        let relation_id = Uuid::now_v7();
        connection
            .execute(
                "INSERT INTO memory_relations (relation_id, subject_type, subject_id, predicate, object_type, object_id, confidence, status, source_type, source_id, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11)",
                params![
                    relation_id.to_string(),
                    input.subject_type.as_storage(),
                    input.subject_id.to_string(),
                    input.predicate,
                    input.object_type.as_storage(),
                    input.object_id.to_string(),
                    f64::from(input.confidence.clamp(0.0, 1.0)),
                    status.as_storage(),
                    input.source_type.as_storage(),
                    input.source_id,
                    now,
                ],
            )
            .map_err(|error| storage_error("MEMORY_RELATION_WRITE_FAILED", error, true))?;
        Ok(relation_id)
    }

    pub fn memory_relation_by_id(
        &self,
        relation_id: Uuid,
    ) -> Result<Option<MemoryRelation>, AppError> {
        let connection = self.connect()?;
        let row = connection
            .query_row(
                "SELECT relation_id, subject_type, subject_id, predicate, object_type, object_id, confidence, status, source_type, source_id, created_at, updated_at FROM memory_relations WHERE relation_id = ?1",
                params![relation_id.to_string()],
                map_memory_relation_row,
            );
        match row {
            Ok(relation) => Ok(Some(relation)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(storage_error("MEMORY_RELATION_READ_FAILED", error, true)),
        }
    }

    /// 更新关系状态（确认候选 / 拒绝 / 标记失效），返回是否真的变了。
    pub fn update_memory_relation_status(
        &self,
        relation_id: Uuid,
        status: MemoryStatus,
    ) -> Result<bool, AppError> {
        let connection = self.connect()?;
        let changed = connection
            .execute(
                "UPDATE memory_relations SET status = ?2, updated_at = ?3 WHERE relation_id = ?1",
                params![
                    relation_id.to_string(),
                    status.as_storage(),
                    Utc::now().to_rfc3339()
                ],
            )
            .map_err(|error| storage_error("MEMORY_RELATION_WRITE_FAILED", error, true))?;
        Ok(changed > 0)
    }

    /// 按主体列出关系（可过滤状态）；limit 保护关系库过大。
    pub fn list_memory_relations_by_subject(
        &self,
        subject_type: MemoryTargetType,
        subject_id: Uuid,
        status: Option<MemoryStatus>,
        limit: u32,
    ) -> Result<Vec<MemoryRelation>, AppError> {
        self.list_memory_relations(
            "subject_type = ?1 AND subject_id = ?2",
            vec![
                SqlValue::Text(subject_type.as_storage().to_owned()),
                SqlValue::Text(subject_id.to_string()),
            ],
            status,
            limit,
        )
    }

    /// 按客体列出关系（反向查找：「这个文件关联了谁」）。
    pub fn list_memory_relations_by_object(
        &self,
        object_type: MemoryTargetType,
        object_id: Uuid,
        status: Option<MemoryStatus>,
        limit: u32,
    ) -> Result<Vec<MemoryRelation>, AppError> {
        self.list_memory_relations(
            "object_type = ?1 AND object_id = ?2",
            vec![
                SqlValue::Text(object_type.as_storage().to_owned()),
                SqlValue::Text(object_id.to_string()),
            ],
            status,
            limit,
        )
    }

    /// 列出全部候选关系（Memory Writer 确定性与校验用）。
    pub fn list_memory_relation_candidates(
        &self,
        limit: u32,
    ) -> Result<Vec<MemoryRelation>, AppError> {
        self.list_memory_relations("status = 'candidate'", Vec::new(), None, limit)
    }

    pub fn list_memory_relations(
        &self,
        where_conditions: &str,
        mut params: Vec<SqlValue>,
        status: Option<MemoryStatus>,
        limit: u32,
    ) -> Result<Vec<MemoryRelation>, AppError> {
        let connection = self.connect()?;
        let status_clause = if status.is_some() {
            " AND status = ?3"
        } else {
            ""
        };
        if let Some(status) = status {
            params.push(SqlValue::Text(status.as_storage().to_owned()));
        }
        let sql = format!(
            "SELECT relation_id, subject_type, subject_id, predicate, object_type, object_id, confidence, status, source_type, source_id, created_at, updated_at FROM memory_relations WHERE {where_conditions}{status_clause} ORDER BY updated_at DESC LIMIT ?{}",
            params.len() + 1
        );
        params.push(SqlValue::Integer(limit as i64));
        let mut statement = connection
            .prepare(&sql)
            .map_err(|error| storage_error("MEMORY_RELATION_READ_FAILED", error, true))?;
        let rows = statement
            .query_map(params_from_iter(params), map_memory_relation_row)
            .map_err(|error| storage_error("MEMORY_RELATION_READ_FAILED", error, true))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| storage_error("MEMORY_RELATION_READ_FAILED", error, true))?;
        drop(statement);
        Ok(rows)
    }

    /// 删除关系。
    pub fn delete_memory_relation(&self, relation_id: Uuid) -> Result<bool, AppError> {
        let connection = self.connect()?;
        let changed = connection
            .execute(
                "DELETE FROM memory_relations WHERE relation_id = ?1",
                params![relation_id.to_string()],
            )
            .map_err(|error| storage_error("MEMORY_RELATION_DELETE_FAILED", error, true))?;
        Ok(changed > 0)
    }

    /// 写入（或按来源等级合并）一条别名。别名必须规范化后非空。
    pub fn upsert_memory_alias(&self, input: &MemoryWriteInput) -> Result<Uuid, AppError> {
        let alias = input
            .alias
            .as_deref()
            .and_then(normalize_alias)
            .ok_or_else(|| AppError::new("MEMORY_ALIAS_INVALID", "别名不能为空", false))?;
        let connection = self.connect()?;
        let now = Utc::now().to_rfc3339();
        let existing: Option<(String, MemorySource, MemoryStatus)> = connection
            .query_row(
                "SELECT alias_id, source_type, status FROM memory_aliases WHERE alias = ?1 AND target_type = ?2 AND target_id = ?3",
                params![alias, input.subject_type.as_storage(), input.subject_id.to_string()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        MemorySource::parse_storage(&row.get::<_, String>(1)?),
                        MemoryStatus::parse_storage(&row.get::<_, String>(2)?),
                    ))
                },
            )
            .optional()
            .map_err(|error| storage_error("MEMORY_ALIAS_READ_FAILED", error, true))?;
        if let Some((alias_id, existing_source, existing_status)) = existing {
            if input.source_type.rank() >= existing_source.rank() {
                // 用户已拒绝的别名不被推断写入「复活」：rejected 状态保持，
                // 只刷新来源/置信度；其余情况按写入方状态更新。
                let next_status = if existing_status == MemoryStatus::Rejected {
                    existing_status.as_storage()
                } else {
                    input.status.as_storage()
                };
                connection
                    .execute(
                        "UPDATE memory_aliases SET confidence = ?2, source_type = ?3, source_id = ?4, status = ?6, updated_at = ?5 WHERE alias_id = ?1",
                        params![
                            alias_id,
                            f64::from(input.confidence.clamp(0.0, 1.0)),
                            input.source_type.as_storage(),
                            input.source_id,
                            now,
                            next_status,
                        ],
                    )
                    .map_err(|error| storage_error("MEMORY_ALIAS_WRITE_FAILED", error, true))?;
            }
            return Uuid::parse_str(&alias_id)
                .map_err(|error| AppError::new("MEMORY_ALIAS_INVALID", error.to_string(), false));
        }
        let alias_id = Uuid::now_v7();
        connection
            .execute(
                "INSERT INTO memory_aliases (alias_id, alias, target_type, target_id, confidence, status, source_type, source_id, hit_count, last_used_at, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0, NULL, ?9, ?9)",
                params![
                    alias_id.to_string(),
                    alias,
                    input.subject_type.as_storage(),
                    input.subject_id.to_string(),
                    f64::from(input.confidence.clamp(0.0, 1.0)),
                    input.status.as_storage(),
                    input.source_type.as_storage(),
                    input.source_id,
                    now,
                ],
            )
            .map_err(|error| storage_error("MEMORY_ALIAS_WRITE_FAILED", error, true))?;
        Ok(alias_id)
    }

    /// 别名 confirm / reject（Phase 4.2「待确认的记忆」）。确认时同时把
    /// 来源升级为 user_confirmed（用户明确确认候选），拒绝后不再参与
    /// Memory Resolution（resolver 只取 confirmed）。
    pub fn update_memory_alias_status(
        &self,
        alias_id: Uuid,
        status: MemoryStatus,
    ) -> Result<bool, AppError> {
        let connection = self.connect()?;
        let now = Utc::now().to_rfc3339();
        let changed = connection
            .execute(
                &format!(
                    "UPDATE memory_aliases SET status = ?2, updated_at = ?3{} WHERE alias_id = ?1",
                    if status == MemoryStatus::Confirmed {
                        ", source_type = 'user_confirmed'"
                    } else {
                        ""
                    }
                ),
                params![alias_id.to_string(), status.as_storage(), now],
            )
            .map_err(|error| storage_error("MEMORY_ALIAS_WRITE_FAILED", error, true))?;
        Ok(changed > 0)
    }

    /// 别名精确匹配（规范化后等值）。按置信度、使用次数排序返回全部候选，
    /// 由调用方（Memory Resolver）决定是否足够明确。
    pub fn find_memory_aliases(&self, alias: &str) -> Result<Vec<MemoryAlias>, AppError> {
        let Some(alias) = normalize_alias(alias) else {
            return Ok(Vec::new());
        };
        let connection = self.connect()?;
        let mut statement = connection
            .prepare(
                "SELECT alias_id, alias, target_type, target_id, confidence, source_type, source_id, hit_count, last_used_at, created_at, updated_at, status FROM memory_aliases WHERE alias = ?1 ORDER BY confidence DESC, hit_count DESC, updated_at DESC LIMIT 20",
            )
            .map_err(|error| storage_error("MEMORY_ALIAS_READ_FAILED", error, true))?;
        let rows = statement
            .query_map(params![alias], map_memory_alias_row)
            .map_err(|error| storage_error("MEMORY_ALIAS_READ_FAILED", error, true))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| storage_error("MEMORY_ALIAS_READ_FAILED", error, true))?;
        drop(statement);
        Ok(rows)
    }

    pub fn memory_alias_by_id(&self, alias_id: Uuid) -> Result<Option<MemoryAlias>, AppError> {
        let connection = self.connect()?;
        let row = connection
            .query_row(
                "SELECT alias_id, alias, target_type, target_id, confidence, source_type, source_id, hit_count, last_used_at, created_at, updated_at, status FROM memory_aliases WHERE alias_id = ?1",
                params![alias_id.to_string()],
                map_memory_alias_row,
            );
        match row {
            Ok(alias) => Ok(Some(alias)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(storage_error("MEMORY_ALIAS_READ_FAILED", error, true)),
        }
    }

    /// 别名被解析命中：hit_count + 1 并刷新 last_used_at（repeated_usage 升级依据）。
    pub fn bump_memory_alias(&self, alias_id: Uuid) -> Result<bool, AppError> {
        let connection = self.connect()?;
        let changed = connection
            .execute(
                "UPDATE memory_aliases SET hit_count = hit_count + 1, last_used_at = ?2 WHERE alias_id = ?1",
                params![alias_id.to_string(), Utc::now().to_rfc3339()],
            )
            .map_err(|error| storage_error("MEMORY_ALIAS_WRITE_FAILED", error, true))?;
        Ok(changed > 0)
    }

    pub fn list_memory_aliases(&self, limit: u32) -> Result<Vec<MemoryAlias>, AppError> {
        let connection = self.connect()?;
        let mut statement = connection
            .prepare(
                "SELECT alias_id, alias, target_type, target_id, confidence, source_type, source_id, hit_count, last_used_at, created_at, updated_at, status FROM memory_aliases ORDER BY updated_at DESC LIMIT ?1",
            )
            .map_err(|error| storage_error("MEMORY_ALIAS_READ_FAILED", error, true))?;
        let rows = statement
            .query_map(params![limit as i64], map_memory_alias_row)
            .map_err(|error| storage_error("MEMORY_ALIAS_READ_FAILED", error, true))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| storage_error("MEMORY_ALIAS_READ_FAILED", error, true))?;
        drop(statement);
        Ok(rows)
    }

    /// 删除别名。
    pub fn delete_memory_alias(&self, alias_id: Uuid) -> Result<bool, AppError> {
        let connection = self.connect()?;
        let changed = connection
            .execute(
                "DELETE FROM memory_aliases WHERE alias_id = ?1",
                params![alias_id.to_string()],
            )
            .map_err(|error| storage_error("MEMORY_ALIAS_DELETE_FAILED", error, true))?;
        Ok(changed > 0)
    }

    /// 清空全部记忆（aliases / relations / entities 三表）。
    /// Phase 3 调试用：只影响 Memory 层，不动文件 / 索引 / Embedding / 会话。
    pub fn clear_memory(&self) -> Result<u64, AppError> {
        let _permit = self.acquire_write(WritePriority::Background);
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction()
            .map_err(|error| storage_error("MEMORY_CLEAR_FAILED", error, true))?;
        let mut deleted = 0_u64;
        for table in ["memory_aliases", "memory_relations", "memory_entities"] {
            deleted = deleted.saturating_add(
                transaction
                    .execute(&format!("DELETE FROM {table}"), [])
                    .map_err(|error| storage_error("MEMORY_CLEAR_FAILED", error, true))?
                    as u64,
            );
        }
        transaction
            .commit()
            .map_err(|error| storage_error("MEMORY_CLEAR_FAILED", error, true))?;
        Ok(deleted)
    }

    /// 文件被删除/失效（Step 16）：引用该文件的关系 → stale（不删除——
    /// 用户以后可能重新添加同名文件并手动确认，stale 允许复活）；
    /// 指向该文件的别名 → 删除（别名失去目标的解析只会造成误导）。
    pub fn invalidate_memory_for_file(&self, file_id: Uuid) -> Result<u64, AppError> {
        let connection = self.connect()?;
        let stale = connection
            .execute(
                "UPDATE memory_relations SET status = 'stale', updated_at = ?2 WHERE (subject_type = 'file' AND subject_id = ?1) OR (object_type = 'file' AND object_id = ?1)",
                params![file_id.to_string(), Utc::now().to_rfc3339()],
            )
            .map_err(|error| storage_error("MEMORY_RELATION_WRITE_FAILED", error, true))?;
        let deleted = connection
            .execute(
                "DELETE FROM memory_aliases WHERE target_type = 'file' AND target_id = ?1",
                params![file_id.to_string()],
            )
            .map_err(|error| storage_error("MEMORY_ALIAS_DELETE_FAILED", error, true))?;
        Ok(stale as u64 + deleted as u64)
    }

    /// Memory Resolver 合法性检查（Step 4）：目标文件必须真实存在、当前在场
    /// 且位于授权根——与 Document Resolver 同口径。别名/关系解析出的 file_id
    /// 绝不绕过这道检查：指向已删除/离线/越权文件的别名直接失效，不注入 scope。
    pub fn memory_file_target_valid(&self, file_id: Uuid) -> Result<bool, AppError> {
        let connection = self.connect()?;
        let valid: bool = connection
            .query_row(
                &format!(
                    "SELECT EXISTS(SELECT 1 FROM files f WHERE f.file_id = ?1 AND f.availability = 'present' AND {AUTHORIZED_FILE_SQL})"
                ),
                params![file_id.to_string()],
                |row| row.get(0),
            )
            .map_err(|error| storage_error("MEMORY_TARGET_CHECK_FAILED", error, true))?;
        Ok(valid)
    }

    /// Memory Resolver 合法性检查：收藏集必须真实存在。
    pub fn memory_collection_target_valid(&self, collection_id: Uuid) -> Result<bool, AppError> {
        let connection = self.connect()?;
        let valid: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM collections c WHERE c.collection_id = ?1)",
                params![collection_id.to_string()],
                |row| row.get(0),
            )
            .map_err(|error| storage_error("MEMORY_TARGET_CHECK_FAILED", error, true))?;
        Ok(valid)
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
                ocr_attempts: vec![],
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
        let ocr_attempts = ocr_attempts_for_revision(&connection, &revision_id)?;
        let next_offset = truncated.then_some((effective_offset + nodes.len()) as u32);
        Ok(crate::FilePreview {
            file,
            revision_id: Some(revision_id),
            nodes,
            image_assets,
            ocr_attempts,
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

    /// DOCUMENT_SUMMARY 用：按文档顺序读取当前修订的全部 chunk
    /// （含 chunk_id/node_id/text/locator，供分层摘要与逐节证据引用）。
    /// 只返回授权 + present + 当前修订的 chunk；文件无当前修订时返回空。
    pub fn file_chunks(&self, file_id: &Uuid) -> Result<Vec<crate::ContentChunk>, AppError> {
        let connection = self.connect()?;
        let file = authorized_file_by_id(&connection, file_id)?;
        let Some(revision_id) = file.current_revision_id else {
            return Ok(Vec::new());
        };
        let mut statement = connection
            .prepare(
                "SELECT chunk_id, revision_id, node_id, ordinal, text, normalized_text, \
                        token_count, content_hash, language, locator_json, \
                        embedding_model_id, embedding_status \
                 FROM chunks WHERE file_id = ?1 AND revision_id = ?2 ORDER BY ordinal",
            )
            .map_err(|error| storage_error("FILE_CHUNKS_QUERY_FAILED", error, true))?;
        let rows = statement
            .query_map(
                params![file_id.to_string(), revision_id.to_string()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, u64>(6)?,
                        row.get::<_, String>(7)?,
                        row.get::<_, String>(8)?,
                        row.get::<_, String>(9)?,
                        row.get::<_, Option<String>>(10)?,
                        row.get::<_, String>(11)?,
                    ))
                },
            )
            .map_err(|error| storage_error("FILE_CHUNKS_QUERY_FAILED", error, true))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| storage_error("FILE_CHUNKS_QUERY_FAILED", error, true))?;
        let mut chunks = Vec::with_capacity(rows.len());
        for (
            chunk_id,
            revision_id,
            node_id,
            ordinal,
            text,
            normalized_text,
            token_count,
            content_hash,
            language,
            locator_json,
            embedding_model_id,
            embedding_status,
        ) in rows
        {
            let Ok(locator) = serde_json::from_str::<SourceLocator>(&locator_json) else {
                continue;
            };
            chunks.push(crate::ContentChunk {
                chunk_id: parse_uuid_value(&chunk_id)?,
                revision_id: parse_uuid_value(&revision_id)?,
                node_id: parse_uuid_value(&node_id)?,
                ordinal: ordinal as u64,
                text,
                normalized_text,
                token_count,
                content_hash,
                language,
                locator,
                embedding_model_id,
                embedding_status,
            });
        }
        Ok(chunks)
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
        let authorized_file = "EXISTS (SELECT 1 FROM file_root_memberships m JOIN roots r ON r.root_id = m.root_id WHERE m.file_id = f.file_id AND r.enabled = 1)";
        let indexable_files = count_query(
            &connection,
            &format!(
                "SELECT COUNT(*) FROM files f WHERE f.availability = 'present' AND f.processing_disposition IN ('parseable_content','image_ocr','read_only_text','archive_manifest') AND {authorized_file}"
            ),
        )?;
        let parsed_files = count_query(
            &connection,
            &format!(
                "SELECT COUNT(*) FROM files f WHERE f.availability = 'present' AND f.parse_status = 'parsed' AND f.processing_disposition IN ('parseable_content','image_ocr','read_only_text','archive_manifest') AND {authorized_file}"
            ),
        )?;
        let searchable_chunks = count_query(
            &connection,
            &format!(
                "SELECT COUNT(*) FROM chunks c JOIN files f ON f.file_id = c.file_id WHERE f.current_revision_id = c.revision_id AND f.availability = 'present' AND {authorized_file}"
            ),
        )?;
        let embedded_chunks = count_query(
            &connection,
            &format!(
                "SELECT COUNT(*) FROM chunk_embeddings e JOIN chunks c ON c.chunk_id = e.chunk_id AND c.file_id = e.file_id AND c.revision_id = e.revision_id JOIN files f ON f.file_id = e.file_id WHERE f.current_revision_id = e.revision_id AND f.availability = 'present' AND {authorized_file}"
            ),
        )?;
        let embedded_files = count_query(
            &connection,
            &format!(
                "SELECT COUNT(DISTINCT e.file_id) FROM chunk_embeddings e JOIN chunks c ON c.chunk_id = e.chunk_id AND c.file_id = e.file_id AND c.revision_id = e.revision_id JOIN files f ON f.file_id = e.file_id WHERE f.current_revision_id = e.revision_id AND f.availability = 'present' AND {authorized_file}"
            ),
        )?;
        let active_vector_keys = count_query(
            &connection,
            &format!(
                "SELECT COUNT(*) FROM vector_index_keys k JOIN index_generations g ON g.generation_id = k.generation_id AND g.status = 'active' JOIN chunks c ON c.chunk_id = k.chunk_id AND c.file_id = k.file_id AND c.revision_id = k.revision_id JOIN files f ON f.file_id = k.file_id WHERE f.current_revision_id = k.revision_id AND f.availability = 'present' AND {authorized_file}"
            ),
        )?;
        let active_index_files = count_query(
            &connection,
            &format!(
                "SELECT COUNT(DISTINCT k.file_id) FROM vector_index_keys k JOIN index_generations g ON g.generation_id = k.generation_id AND g.status = 'active' JOIN chunks c ON c.chunk_id = k.chunk_id AND c.file_id = k.file_id AND c.revision_id = k.revision_id JOIN files f ON f.file_id = k.file_id WHERE f.current_revision_id = k.revision_id AND f.availability = 'present' AND {authorized_file}"
            ),
        )?;
        let indexed_files = active_index_files;
        let pending_files = count_query(
            &connection,
            &format!(
                "SELECT COUNT(*) FROM files f WHERE f.availability = 'present' AND f.parse_status IN ('pending','parsing','ocr_pending') AND {authorized_file}"
            ),
        )?;
        let failed_files = count_query(
            &connection,
            &format!(
                "SELECT COUNT(*) FROM files f WHERE f.availability = 'present' AND f.parse_status IN ('failed','unsupported','encrypted') AND {authorized_file}"
            ),
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
                detail: "维护操作只作用于翻翻索引与日志".into(),
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
        // The UI and background workers poll this method. Keep it a pure read:
        // persisting derived resource state here made status polling contend
        // with parse and embedding transactions.
        let (degradation_level, degradation_reasons) = if pending_files > 500 || active_jobs > 3 {
            (
                "balanced".to_owned(),
                vec!["后台处理队列较长，已优先保证搜索与预览".to_owned()],
            )
        } else {
            ("full".to_owned(), Vec::new())
        };
        let background_notice = degradation_reasons
            .first()
            .cloned()
            .or_else(|| (active_jobs > 0).then(|| format!("{active_jobs}个后台任务正在处理")));
        Ok(MaintenanceSnapshot {
            schema_version: CURRENT_SCHEMA_VERSION,
            database_size_bytes,
            indexed_files,
            indexable_files,
            parsed_files,
            embedded_files,
            active_index_files,
            searchable_chunks,
            embedded_chunks,
            active_vector_keys,
            pending_files,
            failed_files,
            active_jobs,
            log_events,
            degradation_level,
            degradation_reasons,
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

    /// 记录一条节点追踪（明文存储，不走 sanitize_log_value；超 2 万条自动裁剪最旧记录）。
    /// 若当前线程绑定了 OperationTrace（见 crate::active_operation_trace），
    /// 自动把 operation_id 写入节点记录，实现 TraceNode → OperationTrace 关联。
    #[allow(clippy::too_many_arguments)]
    pub fn record_node_trace(
        &self,
        flow: &str,
        node: &str,
        correlation_id: &str,
        session_id: Option<&str>,
        entity_id: Option<&str>,
        input_json: &serde_json::Value,
        output_json: &serde_json::Value,
        status: &str,
        elapsed_ms: Option<u64>,
    ) -> Result<(), AppError> {
        let meta = TraceNodeMeta {
            operation_id: crate::active_operation_trace(),
            ..TraceNodeMeta::default()
        };
        self.record_node_trace_with_meta(
            flow,
            node,
            correlation_id,
            session_id,
            entity_id,
            input_json,
            output_json,
            status,
            elapsed_ms,
            &meta,
        )
    }

    /// 记录一条节点追踪（扩展版：额外写入 operation/评测/设备元数据）。
    /// 新增列可空，未提供的字段写 NULL，与既有记录完全兼容。
    #[allow(clippy::too_many_arguments)]
    pub fn record_node_trace_with_meta(
        &self,
        flow: &str,
        node: &str,
        correlation_id: &str,
        session_id: Option<&str>,
        entity_id: Option<&str>,
        input_json: &serde_json::Value,
        output_json: &serde_json::Value,
        status: &str,
        elapsed_ms: Option<u64>,
        meta: &TraceNodeMeta,
    ) -> Result<(), AppError> {
        let _permit = self.acquire_write(WritePriority::Background);
        let mut connection = self.connect()?;
        // 单事务提交：INSERT + COUNT + 条件裁剪只落一次盘
        let transaction = connection
            .transaction()
            .map_err(|error| storage_error("NODE_TRACE_WRITE_FAILED", error, true))?;
        transaction
            .execute(
                "INSERT INTO node_traces (trace_id, flow, node, correlation_id, session_id, entity_id, input_json, output_json, status, elapsed_ms, created_at, operation_id, evaluation_case_id, optimization_round, model_id, requested_device, actual_device) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
                params![
                    Uuid::now_v7().to_string(),
                    flow,
                    node,
                    correlation_id,
                    session_id,
                    entity_id,
                    serde_json::to_string(input_json)
                        .map_err(|error| AppError::new("NODE_TRACE_SERIALIZE_FAILED", error.to_string(), false))?,
                    serde_json::to_string(output_json)
                        .map_err(|error| AppError::new("NODE_TRACE_SERIALIZE_FAILED", error.to_string(), false))?,
                    status,
                    elapsed_ms.map(i64::try_from).transpose().map_err(|error| {
                        AppError::new("NODE_TRACE_ELAPSED_INVALID", error.to_string(), false)
                    })?,
                    Utc::now().to_rfc3339(),
                    meta.operation_id.as_deref(),
                    meta.evaluation_case_id.as_deref(),
                    meta.optimization_round.map(i64::from),
                    meta.model_id.as_deref(),
                    meta.requested_device.as_deref(),
                    meta.actual_device.as_deref(),
                ],
            )
            .map_err(|error| storage_error("NODE_TRACE_WRITE_FAILED", error, true))?;
        // 超 2 万条才裁剪（复合索引支撑 ORDER BY，避免每次插入都做全表排序）
        let count = transaction
            .query_row("SELECT COUNT(*) FROM node_traces", [], |row| {
                row.get::<_, u64>(0)
            })
            .map_err(|error| storage_error("NODE_TRACE_PRUNE_FAILED", error, true))?;
        if count > 20_000 {
            transaction
                .execute(
                    "DELETE FROM node_traces WHERE trace_id IN (SELECT trace_id FROM node_traces ORDER BY created_at DESC, trace_id DESC LIMIT -1 OFFSET 20000)",
                    [],
                )
                .map_err(|error| storage_error("NODE_TRACE_PRUNE_FAILED", error, true))?;
        }
        transaction
            .commit()
            .map_err(|error| storage_error("NODE_TRACE_WRITE_FAILED", error, true))?;
        Ok(())
    }

    pub fn record_node_trace_input(&self, input: &TraceNodeInput) -> Result<(), AppError> {
        self.record_node_trace_with_meta(
            &input.flow,
            &input.node,
            &input.correlation_id,
            input.session_id.as_deref(),
            input.entity_id.as_deref(),
            &input.input_json,
            &input.output_json,
            &input.status,
            input.elapsed_ms,
            &input.meta,
        )
    }

    /// 新建一条操作级追踪（OperationTrace）。status 固定 running，
    /// completed_at/total_duration_ms 由 complete_operation_trace 补齐。
    /// 返回新生成的 operation_id 供后续完成态回写。
    pub fn record_operation_trace(&self, input: &OperationTraceInput) -> Result<String, AppError> {
        let _permit = self.acquire_write(WritePriority::Background);
        let connection = self.connect()?;
        let operation_id = Uuid::now_v7().to_string();
        connection
            .execute(
                "INSERT INTO operation_traces (operation_id, correlation_id, session_id, feature_type, request, preset_id, status, created_at, completed_at, total_duration_ms, schema_version) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'running', ?7, NULL, NULL, ?8)",
                params![
                    operation_id,
                    input.correlation_id,
                    input.session_id.as_deref(),
                    input.feature_type.as_str(),
                    serde_json::to_string(&input.request)
                        .map_err(|error| AppError::new("OPERATION_TRACE_SERIALIZE_FAILED", error.to_string(), false))?,
                    input.preset_id.as_deref(),
                    Utc::now().to_rfc3339(),
                    OPERATION_TRACE_SCHEMA_VERSION,
                ],
            )
            .map_err(|error| storage_error("OPERATION_TRACE_WRITE_FAILED", error, true))?;
        Ok(operation_id)
    }

    /// 结束一条操作级追踪：补 status/completed_at/total_duration_ms。
    pub fn complete_operation_trace(
        &self,
        operation_id: &str,
        status: &str,
    ) -> Result<(), AppError> {
        let _permit = self.acquire_write(WritePriority::Background);
        let connection = self.connect()?;
        let started_at = connection
            .query_row(
                "SELECT created_at FROM operation_traces WHERE operation_id = ?1",
                [operation_id],
                |row| row.get::<_, String>(0),
            )
            .map_err(|error| storage_error("OPERATION_TRACE_COMPLETE_FAILED", error, true))?;
        let total_ms = DateTime::parse_from_rfc3339(&started_at)
            .ok()
            .map(|started| (Utc::now() - started.with_timezone(&Utc)).num_milliseconds())
            .map(|value| value.max(0) as u64);
        connection
            .execute(
                "UPDATE operation_traces SET status = ?2, completed_at = ?3, total_duration_ms = ?4 WHERE operation_id = ?1",
                params![
                    operation_id,
                    status,
                    Utc::now().to_rfc3339(),
                    total_ms.map(i64::try_from).transpose().map_err(|error| {
                        AppError::new("OPERATION_TRACE_ELAPSED_INVALID", error.to_string(), false)
                    })?,
                ],
            )
            .map_err(|error| storage_error("OPERATION_TRACE_COMPLETE_FAILED", error, true))?;
        Ok(())
    }

    /// 分页查询操作级追踪（按创建时间倒序）。
    pub fn query_operation_traces(
        &self,
        request: &OperationTraceQuery,
    ) -> Result<OperationTracePage, AppError> {
        request.validate()?;
        let offset = request.offset()?;
        let connection = self.connect()?;
        let (count_sql, list_sql, params_count): (&str, &str, usize) = match request
            .feature_type
            .as_deref()
        {
            Some(_feature_type) => (
                "SELECT COUNT(*) FROM operation_traces WHERE feature_type = ?1",
                "SELECT operation_id, correlation_id, session_id, feature_type, request, preset_id, status, created_at, completed_at, total_duration_ms, schema_version FROM operation_traces WHERE feature_type = ?1 ORDER BY created_at DESC, operation_id DESC LIMIT ?2 OFFSET ?3",
                1,
            ),
            None => (
                "SELECT COUNT(*) FROM operation_traces",
                "SELECT operation_id, correlation_id, session_id, feature_type, request, preset_id, status, created_at, completed_at, total_duration_ms, schema_version FROM operation_traces ORDER BY created_at DESC, operation_id DESC LIMIT ?1 OFFSET ?2",
                0,
            ),
        };
        let mut list_params: Vec<SqlValue> = Vec::new();
        if params_count == 1 {
            list_params.push(SqlValue::Text(request.feature_type.clone().unwrap()));
        }
        list_params.push(SqlValue::Integer(i64::from(request.page_size)));
        list_params.push(SqlValue::Integer(i64::try_from(offset).map_err(|_| {
            AppError::new(
                "OPERATION_TRACE_CURSOR_INVALID",
                "操作追踪分页游标无效",
                false,
            )
        })?));
        let total = connection
            .query_row(count_sql, [], |row| row.get::<_, u64>(0))
            .map_err(|error| storage_error("OPERATION_TRACE_QUERY_FAILED", error, true))?;
        let mut statement = connection
            .prepare(list_sql)
            .map_err(|error| storage_error("OPERATION_TRACE_QUERY_FAILED", error, true))?;
        let rows = statement
            .query_map(params_from_iter(list_params), |row| {
                let completed_at: Option<String> = row.get(8)?;
                let total_duration_ms: Option<i64> = row.get(9)?;
                let request: String = row.get(4)?;
                let created_at =
                    parse_datetime_value(&row.get::<_, String>(7)?).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            7,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })?;
                let completed_at = completed_at
                    .as_deref()
                    .map(parse_datetime_value)
                    .transpose()
                    .map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            8,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })?;
                Ok(OperationTraceRecord {
                    operation_id: row.get(0)?,
                    correlation_id: row.get(1)?,
                    session_id: row.get(2)?,
                    feature_type: row.get(3)?,
                    request: serde_json::from_str(&request).unwrap_or(serde_json::Value::Null),
                    preset_id: row.get(5)?,
                    status: row.get(6)?,
                    created_at,
                    completed_at,
                    total_duration_ms: total_duration_ms.map(|value| value as u64),
                    schema_version: row.get::<_, u32>(10)?,
                })
            })
            .map_err(|error| storage_error("OPERATION_TRACE_QUERY_FAILED", error, true))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| storage_error("OPERATION_TRACE_QUERY_FAILED", error, true))?;
        let next_cursor = if offset + (rows.len() as u64) < total {
            Some((offset + rows.len() as u64).to_string())
        } else {
            None
        };
        Ok(OperationTracePage {
            items: rows,
            next_cursor,
            total,
        })
    }

    // ===== 评测闭环持久化（evaluation_cases / evaluation_runs / evaluation_results）=====

    /// 写入一条评测用例（幂等：case_id 冲突时整体覆盖）。
    pub fn record_evaluation_case(&self, case: &EvaluationCaseRecord) -> Result<(), AppError> {
        let _permit = self.acquire_write(WritePriority::Background);
        let connection = self.connect()?;
        connection
            .execute(
                "INSERT INTO evaluation_cases (case_id, feature_type, question_or_request, expected_source, expected_intent, expected_operation, expected_file_ids, expected_chunk_ids, expected_evidence_ids, expected_answer_shape, expected_relation_type, expected_collection_members, gold_reason, split, dataset_version, metadata_json, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17) ON CONFLICT(case_id) DO UPDATE SET feature_type = excluded.feature_type, question_or_request = excluded.question_or_request, expected_source = excluded.expected_source, expected_intent = excluded.expected_intent, expected_operation = excluded.expected_operation, expected_file_ids = excluded.expected_file_ids, expected_chunk_ids = excluded.expected_chunk_ids, expected_evidence_ids = excluded.expected_evidence_ids, expected_answer_shape = excluded.expected_answer_shape, expected_relation_type = excluded.expected_relation_type, expected_collection_members = excluded.expected_collection_members, gold_reason = excluded.gold_reason, split = excluded.split, dataset_version = excluded.dataset_version, metadata_json = excluded.metadata_json, created_at = excluded.created_at",
                params![
                    case.case_id,
                    case.feature_type,
                    case.question_or_request,
                    case.expected_source.as_deref(),
                    case.expected_intent.as_deref(),
                    case.expected_operation.as_deref(),
                    json_list_or_null(case.expected_file_ids.as_ref())?,
                    json_list_or_null(case.expected_chunk_ids.as_ref())?,
                    json_list_or_null(case.expected_evidence_ids.as_ref())?,
                    case.expected_answer_shape.as_deref(),
                    case.expected_relation_type.as_deref(),
                    json_list_or_null(case.expected_collection_members.as_ref())?,
                    case.gold_reason.as_deref(),
                    case.split,
                    case.dataset_version,
                    serde_json::to_string(&case.metadata).map_err(|error| {
                        AppError::new(
                            "EVALUATION_CASE_SERIALIZE_FAILED",
                            error.to_string(),
                            false,
                        )
                    })?,
                    case.created_at.to_rfc3339(),
                ],
            )
            .map_err(|error| storage_error("EVALUATION_CASE_WRITE_FAILED", error, true))?;
        Ok(())
    }

    /// 批量写入评测用例（单事务落盘；返回实际写入条数）。
    pub fn record_evaluation_cases(
        &self,
        cases: &[EvaluationCaseRecord],
    ) -> Result<usize, AppError> {
        if cases.is_empty() {
            return Ok(0);
        }
        let _permit = self.acquire_write(WritePriority::Background);
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction()
            .map_err(|error| storage_error("EVALUATION_CASE_WRITE_FAILED", error, true))?;
        for case in cases {
            transaction
                .execute(
                    "INSERT INTO evaluation_cases (case_id, feature_type, question_or_request, expected_source, expected_intent, expected_operation, expected_file_ids, expected_chunk_ids, expected_evidence_ids, expected_answer_shape, expected_relation_type, expected_collection_members, gold_reason, split, dataset_version, metadata_json, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17) ON CONFLICT(case_id) DO UPDATE SET feature_type = excluded.feature_type, question_or_request = excluded.question_or_request, expected_source = excluded.expected_source, expected_intent = excluded.expected_intent, expected_operation = excluded.expected_operation, expected_file_ids = excluded.expected_file_ids, expected_chunk_ids = excluded.expected_chunk_ids, expected_evidence_ids = excluded.expected_evidence_ids, expected_answer_shape = excluded.expected_answer_shape, expected_relation_type = excluded.expected_relation_type, expected_collection_members = excluded.expected_collection_members, gold_reason = excluded.gold_reason, split = excluded.split, dataset_version = excluded.dataset_version, metadata_json = excluded.metadata_json, created_at = excluded.created_at",
                    params![
                        case.case_id,
                        case.feature_type,
                        case.question_or_request,
                        case.expected_source.as_deref(),
                        case.expected_intent.as_deref(),
                        case.expected_operation.as_deref(),
                        json_list_or_null(case.expected_file_ids.as_ref())?,
                        json_list_or_null(case.expected_chunk_ids.as_ref())?,
                        json_list_or_null(case.expected_evidence_ids.as_ref())?,
                        case.expected_answer_shape.as_deref(),
                        case.expected_relation_type.as_deref(),
                        json_list_or_null(case.expected_collection_members.as_ref())?,
                        case.gold_reason.as_deref(),
                        case.split,
                        case.dataset_version,
                        serde_json::to_string(&case.metadata).map_err(|error| {
                            AppError::new(
                                "EVALUATION_CASE_SERIALIZE_FAILED",
                                error.to_string(),
                                false,
                            )
                        })?,
                        case.created_at.to_rfc3339(),
                    ],
                )
                .map_err(|error| storage_error("EVALUATION_CASE_WRITE_FAILED", error, true))?;
        }
        transaction
            .commit()
            .map_err(|error| storage_error("EVALUATION_CASE_WRITE_FAILED", error, true))?;
        Ok(cases.len())
    }

    /// 新建一条评测执行记录。
    pub fn record_evaluation_run(&self, run: &EvaluationRunRecord) -> Result<(), AppError> {
        let _permit = self.acquire_write(WritePriority::Background);
        let connection = self.connect()?;
        connection
            .execute(
                "INSERT INTO evaluation_runs (run_id, dataset_version, code_revision, preset_id, model_ids, optimization_round, started_at, completed_at, metrics_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, NULL) ON CONFLICT(run_id) DO UPDATE SET dataset_version = excluded.dataset_version, code_revision = excluded.code_revision, preset_id = excluded.preset_id, model_ids = excluded.model_ids, optimization_round = excluded.optimization_round, started_at = excluded.started_at",
                params![
                    run.run_id,
                    run.dataset_version,
                    run.code_revision.as_deref(),
                    run.preset_id.as_deref(),
                    json_list_or_null(run.model_ids.as_ref())?,
                    i64::from(run.optimization_round),
                    run.started_at.to_rfc3339(),
                ],
            )
            .map_err(|error| storage_error("EVALUATION_RUN_WRITE_FAILED", error, true))?;
        Ok(())
    }

    /// 完成评测执行：补 completed_at 与 metrics_json。
    pub fn complete_evaluation_run(
        &self,
        run_id: &str,
        metrics: &serde_json::Value,
    ) -> Result<(), AppError> {
        let _permit = self.acquire_write(WritePriority::Background);
        let connection = self.connect()?;
        connection
            .execute(
                "UPDATE evaluation_runs SET completed_at = ?2, metrics_json = ?3 WHERE run_id = ?1",
                params![
                    run_id,
                    Utc::now().to_rfc3339(),
                    serde_json::to_string(metrics).map_err(|error| {
                        AppError::new("EVALUATION_RUN_SERIALIZE_FAILED", error.to_string(), false)
                    })?,
                ],
            )
            .map_err(|error| storage_error("EVALUATION_RUN_COMPLETE_FAILED", error, true))?;
        Ok(())
    }

    /// 写入一条评测逐例结果（幂等：result_id 冲突时整体覆盖）。
    pub fn record_evaluation_result(
        &self,
        result: &EvaluationResultRecord,
    ) -> Result<(), AppError> {
        let _permit = self.acquire_write(WritePriority::Background);
        let connection = self.connect()?;
        connection
            .execute(
                "INSERT INTO evaluation_results (result_id, case_id, run_id, operation_id, pass_fail, error_category, diagnosis_reason, actual_source, actual_intent, actual_operation, actual_files, actual_evidence, metrics_json, latency_ms, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15) ON CONFLICT(result_id) DO UPDATE SET case_id = excluded.case_id, run_id = excluded.run_id, operation_id = excluded.operation_id, pass_fail = excluded.pass_fail, error_category = excluded.error_category, diagnosis_reason = excluded.diagnosis_reason, actual_source = excluded.actual_source, actual_intent = excluded.actual_intent, actual_operation = excluded.actual_operation, actual_files = excluded.actual_files, actual_evidence = excluded.actual_evidence, metrics_json = excluded.metrics_json, latency_ms = excluded.latency_ms, created_at = excluded.created_at",
                params![
                    result.result_id,
                    result.case_id,
                    result.run_id,
                    result.operation_id.as_deref(),
                    i64::from(result.pass_fail),
                    result.error_category.as_deref(),
                    result.diagnosis_reason.as_deref(),
                    result.actual_source.as_deref(),
                    result.actual_intent.as_deref(),
                    result.actual_operation.as_deref(),
                    json_list_or_null(result.actual_files.as_ref())?,
                    json_list_or_null(result.actual_evidence.as_ref())?,
                    serde_json::to_string(&result.metrics).map_err(|error| {
                        AppError::new(
                            "EVALUATION_RESULT_SERIALIZE_FAILED",
                            error.to_string(),
                            false,
                        )
                    })?,
                    result.latency_ms.map(i64::try_from).transpose().map_err(|error| {
                        AppError::new("EVALUATION_RESULT_LATENCY_INVALID", error.to_string(), false)
                    })?,
                    result.created_at.to_rfc3339(),
                ],
            )
            .map_err(|error| storage_error("EVALUATION_RESULT_WRITE_FAILED", error, true))?;
        Ok(())
    }

    /// 批量写入评测逐例结果（单事务落盘；返回实际写入条数）。
    pub fn record_evaluation_results(
        &self,
        results: &[EvaluationResultRecord],
    ) -> Result<usize, AppError> {
        if results.is_empty() {
            return Ok(0);
        }
        let _permit = self.acquire_write(WritePriority::Background);
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction()
            .map_err(|error| storage_error("EVALUATION_RESULT_WRITE_FAILED", error, true))?;
        for result in results {
            transaction
                .execute(
                    "INSERT INTO evaluation_results (result_id, case_id, run_id, operation_id, pass_fail, error_category, diagnosis_reason, actual_source, actual_intent, actual_operation, actual_files, actual_evidence, metrics_json, latency_ms, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15) ON CONFLICT(result_id) DO UPDATE SET case_id = excluded.case_id, run_id = excluded.run_id, operation_id = excluded.operation_id, pass_fail = excluded.pass_fail, error_category = excluded.error_category, diagnosis_reason = excluded.diagnosis_reason, actual_source = excluded.actual_source, actual_intent = excluded.actual_intent, actual_operation = excluded.actual_operation, actual_files = excluded.actual_files, actual_evidence = excluded.actual_evidence, metrics_json = excluded.metrics_json, latency_ms = excluded.latency_ms, created_at = excluded.created_at",
                    params![
                        result.result_id,
                        result.case_id,
                        result.run_id,
                        result.operation_id.as_deref(),
                        i64::from(result.pass_fail),
                        result.error_category.as_deref(),
                        result.diagnosis_reason.as_deref(),
                        result.actual_source.as_deref(),
                        result.actual_intent.as_deref(),
                        result.actual_operation.as_deref(),
                        json_list_or_null(result.actual_files.as_ref())?,
                        json_list_or_null(result.actual_evidence.as_ref())?,
                        serde_json::to_string(&result.metrics).map_err(|error| {
                            AppError::new(
                                "EVALUATION_RESULT_SERIALIZE_FAILED",
                                error.to_string(),
                                false,
                            )
                        })?,
                        result.latency_ms.map(i64::try_from).transpose().map_err(|error| {
                            AppError::new(
                                "EVALUATION_RESULT_LATENCY_INVALID",
                                error.to_string(),
                                false,
                            )
                        })?,
                        result.created_at.to_rfc3339(),
                    ],
                )
                .map_err(|error| storage_error("EVALUATION_RESULT_WRITE_FAILED", error, true))?;
        }
        transaction
            .commit()
            .map_err(|error| storage_error("EVALUATION_RESULT_WRITE_FAILED", error, true))?;
        Ok(results.len())
    }

    /// 按 split（DEV/HOLDOUT）与可选 feature_type 查询评测用例（创建时间正序）。
    pub fn query_evaluation_cases(
        &self,
        split: &str,
        feature_type: Option<&str>,
    ) -> Result<Vec<EvaluationCaseRecord>, AppError> {
        let connection = self.connect()?;
        let mut statement = match feature_type {
            Some(_feature_type) => {
                connection
                    .prepare(
                        "SELECT case_id, feature_type, question_or_request, expected_source, expected_intent, expected_operation, expected_file_ids, expected_chunk_ids, expected_evidence_ids, expected_answer_shape, expected_relation_type, expected_collection_members, gold_reason, split, dataset_version, metadata_json, created_at FROM evaluation_cases WHERE split = ?1 AND feature_type = ?2 ORDER BY created_at ASC, case_id ASC",
                    )
                    .map_err(|error| storage_error("EVALUATION_CASE_QUERY_FAILED", error, true))?
            }
            None => connection
                .prepare(
                    "SELECT case_id, feature_type, question_or_request, expected_source, expected_intent, expected_operation, expected_file_ids, expected_chunk_ids, expected_evidence_ids, expected_answer_shape, expected_relation_type, expected_collection_members, gold_reason, split, dataset_version, metadata_json, created_at FROM evaluation_cases WHERE split = ?1 ORDER BY created_at ASC, case_id ASC",
                )
                .map_err(|error| storage_error("EVALUATION_CASE_QUERY_FAILED", error, true))?,
        };
        let cases = match feature_type {
            Some(feature_type) => statement
                .query_map(params![split, feature_type], read_evaluation_case_row)
                .map_err(|error| storage_error("EVALUATION_CASE_QUERY_FAILED", error, true))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| storage_error("EVALUATION_CASE_QUERY_FAILED", error, true))?,
            None => statement
                .query_map(params![split], read_evaluation_case_row)
                .map_err(|error| storage_error("EVALUATION_CASE_QUERY_FAILED", error, true))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| storage_error("EVALUATION_CASE_QUERY_FAILED", error, true))?,
        };
        Ok(cases)
    }

    /// 查询评测执行记录（可按 optimization_round 过滤；按 started_at 倒序）。
    pub fn query_evaluation_runs(
        &self,
        optimization_round: Option<u32>,
    ) -> Result<Vec<EvaluationRunRecord>, AppError> {
        let connection = self.connect()?;
        let mut statement = match optimization_round {
            Some(_round) => connection
                .prepare(
                    "SELECT run_id, dataset_version, code_revision, preset_id, model_ids, optimization_round, started_at, completed_at, metrics_json FROM evaluation_runs WHERE optimization_round = ?1 ORDER BY started_at DESC, run_id DESC",
                )
                .map_err(|error| storage_error("EVALUATION_RUN_QUERY_FAILED", error, true))?,
            None => connection
                .prepare(
                    "SELECT run_id, dataset_version, code_revision, preset_id, model_ids, optimization_round, started_at, completed_at, metrics_json FROM evaluation_runs ORDER BY started_at DESC, run_id DESC",
                )
                .map_err(|error| storage_error("EVALUATION_RUN_QUERY_FAILED", error, true))?,
        };
        let runs = match optimization_round {
            Some(round) => statement
                .query_map(params![i64::from(round)], read_evaluation_run_row)
                .map_err(|error| storage_error("EVALUATION_RUN_QUERY_FAILED", error, true))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| storage_error("EVALUATION_RUN_QUERY_FAILED", error, true))?,
            None => statement
                .query_map([], read_evaluation_run_row)
                .map_err(|error| storage_error("EVALUATION_RUN_QUERY_FAILED", error, true))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| storage_error("EVALUATION_RUN_QUERY_FAILED", error, true))?,
        };
        Ok(runs)
    }

    /// 查询某次评测执行的全部逐例结果（按 created_at 正序）。
    pub fn query_evaluation_results(
        &self,
        run_id: &str,
    ) -> Result<Vec<EvaluationResultRecord>, AppError> {
        let connection = self.connect()?;
        let mut statement = connection
            .prepare(
                "SELECT result_id, case_id, run_id, operation_id, pass_fail, error_category, diagnosis_reason, actual_source, actual_intent, actual_operation, actual_files, actual_evidence, metrics_json, latency_ms, created_at FROM evaluation_results WHERE run_id = ?1 ORDER BY created_at ASC, result_id ASC",
            )
            .map_err(|error| storage_error("EVALUATION_RESULT_QUERY_FAILED", error, true))?;
        statement
            .query_map(params![run_id], read_evaluation_result_row)
            .map_err(|error| storage_error("EVALUATION_RESULT_QUERY_FAILED", error, true))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| storage_error("EVALUATION_RESULT_QUERY_FAILED", error, true))
    }

    pub fn query_node_traces(&self, request: &NodeTraceQuery) -> Result<NodeTracePage, AppError> {
        request.validate()?;
        let offset = request.offset()?;
        let connection = self.connect()?;
        let (count_sql, filter_params) = match (request.flow.as_deref(), request.node.as_deref()) {
            (Some(flow), Some(node)) => (
                "SELECT COUNT(*) FROM node_traces WHERE flow = ?1 AND node = ?2",
                vec![SqlValue::Text(flow.into()), SqlValue::Text(node.into())],
            ),
            (Some(flow), None) => (
                "SELECT COUNT(*) FROM node_traces WHERE flow = ?1",
                vec![SqlValue::Text(flow.into())],
            ),
            (None, Some(node)) => (
                "SELECT COUNT(*) FROM node_traces WHERE node = ?1",
                vec![SqlValue::Text(node.into())],
            ),
            (None, None) => ("SELECT COUNT(*) FROM node_traces", vec![]),
        };
        let total = connection
            .query_row(
                count_sql,
                params_from_iter(filter_params.iter().cloned()),
                |row| row.get::<_, u64>(0),
            )
            .map_err(|error| storage_error("NODE_TRACE_QUERY_FAILED", error, true))?;
        let (sql, list_params): (&str, Vec<SqlValue>) = match (
            request.flow.as_deref(),
            request.node.as_deref(),
        ) {
            (Some(flow), Some(node)) => (
                "SELECT trace_id, flow, node, correlation_id, session_id, entity_id, input_json, output_json, status, elapsed_ms, created_at FROM node_traces WHERE flow = ?1 AND node = ?2 ORDER BY created_at DESC, trace_id DESC LIMIT ?3 OFFSET ?4",
                vec![SqlValue::Text(flow.into()), SqlValue::Text(node.into())],
            ),
            (Some(flow), None) => (
                "SELECT trace_id, flow, node, correlation_id, session_id, entity_id, input_json, output_json, status, elapsed_ms, created_at FROM node_traces WHERE flow = ?1 ORDER BY created_at DESC, trace_id DESC LIMIT ?2 OFFSET ?3",
                vec![SqlValue::Text(flow.into())],
            ),
            (None, Some(node)) => (
                "SELECT trace_id, flow, node, correlation_id, session_id, entity_id, input_json, output_json, status, elapsed_ms, created_at FROM node_traces WHERE node = ?1 ORDER BY created_at DESC, trace_id DESC LIMIT ?2 OFFSET ?3",
                vec![SqlValue::Text(node.into())],
            ),
            (None, None) => (
                "SELECT trace_id, flow, node, correlation_id, session_id, entity_id, input_json, output_json, status, elapsed_ms, created_at FROM node_traces ORDER BY created_at DESC, trace_id DESC LIMIT ?1 OFFSET ?2",
                vec![],
            ),
        };
        let mut parameters = list_params;
        let page_size = i64::from(request.page_size);
        let offset_i64 = i64::try_from(offset)
            .map_err(|error| AppError::new("NODE_TRACE_QUERY_FAILED", error.to_string(), false))?;
        parameters.push(SqlValue::Integer(page_size));
        parameters.push(SqlValue::Integer(offset_i64));
        let mut statement = connection
            .prepare(sql)
            .map_err(|error| storage_error("NODE_TRACE_QUERY_FAILED", error, true))?;
        let items = statement
            .query_map(params_from_iter(parameters.iter().cloned()), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, Option<i64>>(9)?,
                    row.get::<_, String>(10)?,
                ))
            })
            .map_err(|error| storage_error("NODE_TRACE_QUERY_FAILED", error, true))?
            .map(|row| {
                let (
                    trace_id,
                    flow,
                    node,
                    correlation_id,
                    session_id,
                    entity_id,
                    input_json,
                    output_json,
                    status,
                    elapsed_ms,
                    created_at,
                ) = row.map_err(|error| storage_error("NODE_TRACE_QUERY_FAILED", error, true))?;
                Ok(NodeTraceRecord {
                    trace_id,
                    flow,
                    node,
                    correlation_id,
                    session_id,
                    entity_id,
                    input_json: serde_json::from_str(&input_json).map_err(|error| {
                        AppError::new("NODE_TRACE_DATA_INVALID", error.to_string(), false)
                    })?,
                    output_json: serde_json::from_str(&output_json).map_err(|error| {
                        AppError::new("NODE_TRACE_DATA_INVALID", error.to_string(), false)
                    })?,
                    status,
                    elapsed_ms: elapsed_ms.map(|value| value as u64),
                    created_at: DateTime::parse_from_rfc3339(&created_at)
                        .map_err(|error| {
                            AppError::new("NODE_TRACE_DATA_INVALID", error.to_string(), false)
                        })?
                        .with_timezone(&Utc),
                })
            })
            .collect::<Result<Vec<_>, AppError>>()?;
        let consumed = offset.saturating_add(items.len() as u64);
        Ok(NodeTracePage {
            items,
            next_cursor: (consumed < total).then(|| consumed.to_string()),
            total,
        })
    }

    /// 拉取一次操作（如单次 Ask）的全部节点追踪，按时间正序（流水顺序）。
    /// 单次 Ask 的节点数有限，不做分页；供 Trace Viewer 与 Debug Trace 导出使用。
    pub fn query_node_traces_by_correlation(
        &self,
        flow: &str,
        correlation_id: &str,
    ) -> Result<Vec<NodeTraceRecord>, AppError> {
        let connection = self.connect()?;
        let mut statement = connection
            .prepare(
                "SELECT trace_id, flow, node, correlation_id, session_id, entity_id, input_json, output_json, status, elapsed_ms, created_at FROM node_traces WHERE flow = ?1 AND correlation_id = ?2 ORDER BY created_at ASC, trace_id ASC",
            )
            .map_err(|error| storage_error("NODE_TRACE_QUERY_FAILED", error, true))?;
        let rows = statement
            .query_map(params![flow, correlation_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, Option<i64>>(9)?,
                    row.get::<_, String>(10)?,
                ))
            })
            .map_err(|error| storage_error("NODE_TRACE_QUERY_FAILED", error, true))?
            .map(|row| {
                let (
                    trace_id,
                    flow,
                    node,
                    correlation_id,
                    session_id,
                    entity_id,
                    input_json,
                    output_json,
                    status,
                    elapsed_ms,
                    created_at,
                ) = row.map_err(|error| storage_error("NODE_TRACE_QUERY_FAILED", error, true))?;
                Ok(NodeTraceRecord {
                    trace_id,
                    flow,
                    node,
                    correlation_id,
                    session_id,
                    entity_id,
                    input_json: serde_json::from_str(&input_json).map_err(|error| {
                        AppError::new("NODE_TRACE_DATA_INVALID", error.to_string(), false)
                    })?,
                    output_json: serde_json::from_str(&output_json).map_err(|error| {
                        AppError::new("NODE_TRACE_DATA_INVALID", error.to_string(), false)
                    })?,
                    status,
                    elapsed_ms: elapsed_ms.map(|value| value as u64),
                    created_at: DateTime::parse_from_rfc3339(&created_at)
                        .map_err(|error| {
                            AppError::new("NODE_TRACE_DATA_INVALID", error.to_string(), false)
                        })?
                        .with_timezone(&Utc),
                })
            })
            .collect::<Result<Vec<_>, AppError>>()?;
        Ok(rows)
    }

    pub fn clear_node_traces(&self) -> Result<u64, AppError> {
        let _permit = self.acquire_write(WritePriority::Background);
        let connection = self.connect()?;
        connection
            .execute("DELETE FROM node_traces", [])
            .map(|value| value as u64)
            .map_err(|error| storage_error("NODE_TRACE_CLEAR_FAILED", error, true))
    }

    pub fn rebuild_index(&self, confirmation: &str) -> Result<IndexRebuildResult, AppError> {
        crate::validate_rebuild_confirmation(confirmation)?;
        let reset_files = count_query(
            &self.connect()?,
            "SELECT COUNT(*) FROM files WHERE current_revision_id IS NOT NULL",
        )?;
        loop {
            let _permit = self.acquire_write(WritePriority::Background);
            let mut connection = self.connect()?;
            let transaction = connection
                .transaction()
                .map_err(|error| storage_error("DATABASE_TRANSACTION_FAILED", error, true))?;
            transaction.execute(
                "UPDATE file_revisions SET parse_status = 'pending', parser_name = NULL, parser_version = NULL, index_version = NULL, completed_at = NULL, error_code = NULL WHERE revision_id IN (SELECT current_revision_id FROM files WHERE current_revision_id IS NOT NULL AND parse_status <> 'pending' ORDER BY last_seen_at DESC, file_id DESC LIMIT 250)",
                [],
            ).map_err(|error| storage_error("INDEX_REBUILD_FAILED", error, true))?;
            let marked = transaction.execute(
                "UPDATE files SET parse_status = 'pending' WHERE file_id IN (SELECT file_id FROM files WHERE current_revision_id IS NOT NULL AND parse_status <> 'pending' ORDER BY last_seen_at DESC, file_id DESC LIMIT 250)",
                [],
            ).map_err(|error| storage_error("INDEX_REBUILD_FAILED", error, true))?;
            transaction
                .commit()
                .map_err(|error| storage_error("DATABASE_COMMIT_FAILED", error, true))?;
            if marked == 0 {
                break;
            }
            thread::yield_now();
        }
        Ok(IndexRebuildResult {
            reset_files,
            removed_nodes: 0,
            removed_chunks: 0,
            removed_embeddings: 0,
            source_files_modified: false,
        })
    }

    pub fn record_file_events(
        &self,
        root_id: &Uuid,
        events: &[FileSystemEvent],
    ) -> Result<(), AppError> {
        let _permit = self.acquire_write(WritePriority::Background);
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
        let _permit = self.acquire_write(WritePriority::Background);
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

pub fn sanitize_log_value(
    value: &serde_json::Value,
    field_name: Option<&str>,
) -> serde_json::Value {
    let sensitive_field = field_name.is_some_and(|name| {
        let name = name.to_ascii_lowercase();
        [
            "path", "text", "content", "body", "quote", "query", "prompt", "question", "answer",
            "snippet",
        ]
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
        serde_json::Value::String(text) => serde_json::Value::String(redact_absolute_paths(text)),
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

fn redact_absolute_paths(text: &str) -> String {
    static WINDOWS_PATH: OnceLock<Regex> = OnceLock::new();
    let expression = WINDOWS_PATH.get_or_init(|| {
        Regex::new(
            r#"(?i)(?:\\\\\?\\)?[a-z]:[\\/][^\s\"'<>|,;)\]}]+|\\\\[^\\/\s]+\\[^\s\"'<>|,;)\]}]+"#,
        )
        .expect("absolute path redaction regex")
    });
    expression.replace_all(text, "[redacted_path]").into_owned()
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

#[allow(clippy::too_many_arguments)]
fn replace_image_search_node(
    transaction: &Transaction<'_>,
    file_id: &Uuid,
    revision_id: &Uuid,
    asset_id: &Uuid,
    locator: &SourceLocator,
    node_type: &str,
    heading: &str,
    text: String,
) -> Result<(), AppError> {
    transaction
        .execute(
            "DELETE FROM chunks_fts WHERE chunk_id IN (SELECT chunk_id FROM chunks WHERE node_id IN (SELECT node_id FROM document_nodes WHERE image_asset_id = ?1))",
            [asset_id.to_string()],
        )
        .map_err(|error| storage_error("IMAGE_SEARCH_NODE_WRITE_FAILED", error, true))?;
    transaction
        .execute(
            "DELETE FROM document_nodes WHERE image_asset_id = ?1",
            [asset_id.to_string()],
        )
        .map_err(|error| storage_error("IMAGE_SEARCH_NODE_WRITE_FAILED", error, true))?;
    let node_ordinal = transaction
        .query_row(
            "SELECT COALESCE(MAX(ordinal), 0) + 1 FROM document_nodes WHERE revision_id = ?1",
            [revision_id.to_string()],
            |row| row.get::<_, u64>(0),
        )
        .map_err(|error| storage_error("IMAGE_SEARCH_NODE_WRITE_FAILED", error, true))?;
    let node = DocumentNode {
        node_id: *asset_id,
        parent_id: None,
        ordinal: node_ordinal,
        node_type: node_type.to_owned(),
        text: Some(text),
        table_data: None,
        locator: locator.clone(),
        heading_path: vec![heading.to_owned()],
    };
    let parse_result = ParseResult {
        revision_id: *revision_id,
        status: ParseOutcome::Parsed,
        parser_name: node_type.to_owned(),
        parser_version: "1".into(),
        nodes: vec![node.clone()],
        image_assets: vec![],
        ocr_attempts: vec![],
        warnings: vec![],
        metrics: crate::ParseMetrics {
            page_count: 0,
            node_count: 1,
            character_count: node
                .text
                .as_ref()
                .map_or(0, |value| value.chars().count() as u64),
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
        .map_err(|error| storage_error("IMAGE_SEARCH_NODE_WRITE_FAILED", error, true))?;
    let locator_json = serde_json::to_string(locator)
        .map_err(|error| AppError::new("INDEX_SERIALIZE_FAILED", error.to_string(), false))?;
    transaction
        .execute(
            "INSERT INTO document_nodes (node_id, revision_id, parent_id, ordinal, node_type, locator_json, heading_path_json, text, table_json, image_asset_id) VALUES (?1, ?2, NULL, ?3, ?4, ?5, ?6, ?7, NULL, ?1)",
            params![asset_id.to_string(), revision_id.to_string(), node_ordinal, node_type, locator_json, serde_json::to_string(&node.heading_path).expect("serialize static heading"), node.text],
        )
        .map_err(|error| storage_error("IMAGE_SEARCH_NODE_WRITE_FAILED", error, true))?;
    for chunk in &mut chunks {
        chunk.ordinal += chunk_ordinal;
        let chunk_locator = serde_json::to_string(&chunk.locator)
            .map_err(|error| AppError::new("INDEX_SERIALIZE_FAILED", error.to_string(), false))?;
        transaction
            .execute(
                "INSERT INTO chunks (chunk_id, file_id, revision_id, node_id, ordinal, text, normalized_text, token_count, content_hash, language, locator_json, embedding_model_id, embedding_status) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, NULL, 'pending')",
                params![chunk.chunk_id.to_string(), file_id.to_string(), revision_id.to_string(), asset_id.to_string(), chunk.ordinal, chunk.text, chunk.normalized_text, chunk.token_count, chunk.content_hash, chunk.language, chunk_locator],
            )
            .map_err(|error| storage_error("IMAGE_SEARCH_NODE_WRITE_FAILED", error, true))?;
        transaction
            .execute(
                "INSERT INTO chunks_fts (chunk_id, file_id, revision_id, normalized_text) VALUES (?1, ?2, ?3, ?4)",
                params![chunk.chunk_id.to_string(), file_id.to_string(), revision_id.to_string(), chunk.normalized_text],
            )
            .map_err(|error| storage_error("IMAGE_SEARCH_NODE_WRITE_FAILED", error, true))?;
    }
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

fn inbox_item_by_id(
    connection: &Connection,
    inbox_id: &Uuid,
    collections: &[CollectionRecord],
) -> Result<Option<InboxItem>, AppError> {
    let sql = format!(
        "SELECT f.file_id, f.volume_id, f.canonical_path, f.display_name, f.extension, f.mime_type, f.size_bytes, f.fs_created_at, f.modified_at, f.windows_file_id, f.content_sha256, f.availability, f.current_revision_id, f.parse_status, f.first_seen_at, f.last_seen_at, i.inbox_id, i.event_type, i.observed_at, i.previous_path, i.triage_status, i.summary, i.error_code, i.resolution_status, i.attempt_count, i.last_attempt_at, (SELECT relation_id FROM file_relations r WHERE r.relation_type = 'exact_duplicate' AND (r.left_file_id = f.file_id OR r.right_file_id = f.file_id) AND r.review_status <> 'rejected' ORDER BY r.confidence DESC LIMIT 1) FROM inbox_events i JOIN files f ON f.file_id = i.file_id WHERE i.inbox_id = ?1 AND {AUTHORIZED_FILE_SQL} LIMIT 1"
    );
    let row = connection
        .query_row(&sql, [inbox_id.to_string()], |row| {
            let file = file_from_row(row)?;
            let inbox_id: String = row.get(16)?;
            let event_type: String = row.get(17)?;
            let observed_at: String = row.get(18)?;
            let triage_status: String = row.get(20)?;
            let resolution_status: String = row.get(23)?;
            let last_attempt_at: Option<String> = row.get(25)?;
            let duplicate_group_id: Option<String> = row.get(26)?;
            Ok((
                file,
                parse_uuid_column(&inbox_id, 16)?,
                InboxEventType::from_storage(&event_type),
                parse_datetime_column(&observed_at, 18)?,
                row.get::<_, Option<String>>(19)?,
                TriageStatus::from_storage(&triage_status),
                row.get::<_, Option<String>>(21)?,
                row.get::<_, Option<String>>(22)?,
                ResolutionStatus::from_storage(&resolution_status),
                row.get::<_, u32>(24)?,
                last_attempt_at
                    .map(|value| parse_datetime_column(&value, 25))
                    .transpose()?,
                duplicate_group_id
                    .map(|value| parse_uuid_column(&value, 26))
                    .transpose()?,
            ))
        })
        .optional()
        .map_err(|error| storage_error("INBOX_QUERY_FAILED", error, true))?;
    let Some((
        file,
        inbox_id,
        event_type,
        observed_at,
        previous_path,
        triage_status,
        summary,
        error_code,
        resolution_status,
        attempt_count,
        last_attempt_at,
        duplicate_group_id,
    )) = row
    else {
        return Ok(None);
    };
    let mut suggested_collection_ids = Vec::new();
    for collection in collections {
        if let Some(rule) = &collection.rule
            && collection_rule_matches(connection, rule, &file)?
        {
            suggested_collection_ids.push(collection.collection_id);
        }
    }
    Ok(Some(InboxItem {
        inbox_id,
        file_id: file.file_id,
        display_name: file.display_name,
        canonical_path: file.canonical_path,
        event_type,
        observed_at,
        previous_path,
        triage_status,
        resolution_status,
        attempt_count,
        last_attempt_at,
        retry_action: retry_action_for(event_type, resolution_status),
        suggested_collection_ids,
        duplicate_group_id,
        summary,
        error_code,
    }))
}

fn retry_action_for(
    event_type: InboxEventType,
    resolution_status: ResolutionStatus,
) -> Option<String> {
    if !matches!(
        resolution_status,
        ResolutionStatus::PendingRetry | ResolutionStatus::Retrying
    ) {
        return None;
    }
    match event_type {
        InboxEventType::OcrRequired => Some("retry_ocr".into()),
        InboxEventType::ParseFailed => Some("retry_parse".into()),
        _ => Some("retry_processing".into()),
    }
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
fn file_filter_digest(request: &FileQuery) -> String {
    let mut extensions = request
        .extensions
        .iter()
        .map(|value| value.trim().trim_start_matches('.').to_ascii_lowercase())
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    extensions.sort();
    let mut statuses = request.parse_statuses.clone();
    statuses.sort();
    stable_filter_digest(&serde_json::json!({
        "query": request.query.as_deref().map(str::trim).unwrap_or(""),
        "extensions": extensions,
        "parse_statuses": statuses,
        "availability": request.availability.map(|value| value.as_str()),
    }))
}

fn inbox_filter_digest(query: &InboxQuery) -> String {
    let mut event_types = query
        .event_types
        .iter()
        .map(|value| value.as_storage())
        .collect::<Vec<_>>();
    event_types.sort();
    let mut root_ids = query
        .root_ids
        .iter()
        .map(Uuid::to_string)
        .collect::<Vec<_>>();
    root_ids.sort();
    stable_filter_digest(&serde_json::json!({
        "status": query.status.as_storage(),
        "event_types": event_types,
        "root_ids": root_ids,
        "date_from": query.date_from,
        "date_to": query.date_to,
    }))
}

fn stable_filter_digest(value: &serde_json::Value) -> String {
    let mut digest = Sha256::new();
    digest.update(value.to_string().as_bytes());
    format!("{:x}", digest.finalize())
}

fn encode_keyset_cursor<T: serde::Serialize>(cursor: T) -> Result<String, AppError> {
    let bytes = serde_json::to_vec(&cursor)
        .map_err(|error| AppError::new("CURSOR_SERIALIZE_FAILED", error.to_string(), false))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn ask_session_title(question: &str) -> String {
    let normalized = question.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut title = normalized.chars().take(28).collect::<String>();
    if normalized.chars().count() > 28 {
        title.push('…');
    }
    if title.is_empty() {
        "新会话".into()
    } else {
        title
    }
}

fn decode_keyset_cursor<T: serde::de::DeserializeOwned>(
    encoded: &str,
    error_code: &str,
    message: &str,
) -> Result<T, AppError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| AppError::new(error_code, message, false))?;
    serde_json::from_slice(&bytes).map_err(|_| AppError::new(error_code, message, false))
}

const AUTHORIZED_FILE_SQL: &str = "EXISTS (SELECT 1 FROM file_root_memberships m JOIN roots r ON r.root_id = m.root_id WHERE m.file_id = f.file_id AND r.enabled = 1)";

fn ratio(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        (numerator as f64 / denominator as f64).clamp(0.0, 1.0)
    }
}

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

/// 取一个文件的文档级向量：文件前 3 个 chunk 的均值（与语义关系刷新口径一致）。
/// 带进程内缓存；模型未配置或文件无向量时返回 None。
fn file_vector_for(
    connection: &Connection,
    model_artifact_id: Option<&str>,
    file_id: Uuid,
    cache: &mut HashMap<Uuid, Vec<f32>>,
) -> Result<Option<Vec<f32>>, AppError> {
    if let Some(vector) = cache.get(&file_id) {
        return Ok(Some(vector.clone()));
    }
    let Some(model_artifact_id) = model_artifact_id else {
        return Ok(None);
    };
    let sql = "WITH ranked AS (SELECT ce.vector_blob, ce.dimension, ROW_NUMBER() OVER (PARTITION BY ce.file_id ORDER BY c.ordinal) AS rn FROM chunk_embeddings ce JOIN chunks c ON c.chunk_id = ce.chunk_id WHERE ce.model_artifact_id = ?1 AND ce.file_id = ?2) SELECT vector_blob, dimension FROM ranked WHERE rn <= 3";
    let mut statement = connection
        .prepare(sql)
        .map_err(|error| storage_error("RELATION_VECTOR_QUERY_FAILED", error, true))?;
    let rows = statement
        .query_map(params![model_artifact_id, file_id.to_string()], |row| {
            Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, u32>(1)?))
        })
        .map_err(|error| storage_error("RELATION_VECTOR_QUERY_FAILED", error, true))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| storage_error("RELATION_VECTOR_QUERY_FAILED", error, true))?;
    if rows.is_empty() {
        return Ok(None);
    }
    let mut vectors = Vec::with_capacity(rows.len());
    for (bytes, dimension) in rows {
        vectors.push(decode_vector(&bytes, dimension)?);
    }
    let vector = mean_normalized_vector(&vectors)?;
    cache.insert(file_id, vector.clone());
    Ok(Some(vector))
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

fn deterministic_collection_name(titles: &[String]) -> String {
    let normalized = titles
        .iter()
        .map(|title| collection_name_stem(title))
        .collect::<Vec<_>>();
    let categories = [
        (&["面试", "简历", "招聘"][..], "求职与面试"),
        (&["考试", "题库", "试卷", "复习"][..], "考试与复习"),
        (&["项目", "需求", "产品", "方案"][..], "项目与产品"),
        (&["合同", "协议", "条款"][..], "合同与协议"),
        (&["课程", "教程", "学习", "笔记"][..], "学习与笔记"),
        (&["发票", "报销", "账单"][..], "财务与报销"),
        (&["会议", "纪要", "议程"][..], "会议资料"),
    ];
    let common_category = categories.iter().find_map(|(markers, label)| {
        (normalized
            .iter()
            .filter(|title| markers.iter().any(|marker| title.contains(marker)))
            .count()
            >= 2)
            .then_some(*label)
    });
    let extensions = titles
        .iter()
        .filter_map(|title| {
            title
                .rsplit_once('.')
                .map(|(_, extension)| extension.to_lowercase())
        })
        .collect::<Vec<_>>();
    let type_label = if !extensions.is_empty()
        && extensions
            .iter()
            .all(|extension| extension == &extensions[0])
    {
        match extensions[0].as_str() {
            "pdf" => "PDF资料",
            "doc" | "docx" => "Word文档",
            "xls" | "xlsx" | "csv" => "表格资料",
            "ppt" | "pptx" => "演示资料",
            "png" | "jpg" | "jpeg" | "webp" | "bmp" => "图片资料",
            "rs" | "py" | "js" | "ts" | "tsx" | "java" | "cpp" | "c" | "go" => "代码资料",
            _ => "同类型资料",
        }
    } else {
        "跨格式资料"
    };
    if let Some(category) = common_category {
        return format!("{category} · {type_label}");
    }
    let mut frequency = HashMap::<String, usize>::new();
    for title in &normalized {
        let unique = profile_keywords(title).into_iter().collect::<HashSet<_>>();
        for keyword in unique {
            if !matches!(keyword.as_str(), "资料" | "文档" | "文件" | "最终" | "版本") {
                *frequency.entry(keyword).or_default() += 1;
            }
        }
    }
    let mut common = frequency
        .into_iter()
        .filter(|(_, count)| *count >= 2)
        .collect::<Vec<_>>();
    common.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    if let Some((keyword, _)) = common.first() {
        format!("{keyword}主题 · {type_label}")
    } else {
        format!("共同主题 · {type_label}")
    }
}

fn is_generic_collection_name(value: &str) -> bool {
    matches!(
        value.trim().to_lowercase().as_str(),
        "相关资料" | "相关文档" | "文档集合" | "资料集合" | "其他资料" | "related files"
    )
}

fn is_summary_like_name(value: &str) -> bool {
    let lowered = value.to_lowercase();
    [
        "摘要", "总结", "概览", "提纲", "速览", "要点", "summary", "overview", "outline",
    ]
    .iter()
    .any(|marker| lowered.contains(marker))
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
            "SELECT asset_id, revision_id, asset_kind, cache_path, mime_type, size_bytes, sha256, locator_json, ocr_text, ocr_confidence, ocr_engine, description, vision_model_id, vision_route_reason, status, COALESCE(error_json, ocr_error_json) FROM image_assets WHERE revision_id = ?1 ORDER BY created_at, asset_id LIMIT 512",
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
                ocr_confidence: row.get(9)?,
                ocr_engine: row.get(10)?,
                description: row.get(11)?,
                vision_model_id: row.get(12)?,
                vision_route_reason: row.get(13)?,
                status: row.get(14)?,
                error: row
                    .get::<_, Option<String>>(15)?
                    .map(|value| parse_json_column(15, &value))
                    .transpose()?,
            })
        })
        .map_err(|error| storage_error("IMAGE_ASSET_QUERY_FAILED", error, true))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| storage_error("IMAGE_ASSET_QUERY_FAILED", error, true))
}

fn ocr_attempts_for_revision(
    connection: &Connection,
    revision_id: &Uuid,
) -> Result<Vec<crate::OcrAttempt>, AppError> {
    let mut statement = connection
        .prepare(
            "SELECT engine, model_version, status, page_no, confidence, fallback_reason, elapsed_ms, error_json FROM ocr_attempts WHERE revision_id = ?1 ORDER BY ordinal, attempt_id LIMIT 1024",
        )
        .map_err(|error| storage_error("OCR_ATTEMPT_QUERY_FAILED", error, true))?;
    statement
        .query_map([revision_id.to_string()], |row| {
            Ok(crate::OcrAttempt {
                engine: row.get(0)?,
                model_version: row.get(1)?,
                status: row.get(2)?,
                page_no: row.get(3)?,
                confidence: row.get(4)?,
                fallback_reason: row.get(5)?,
                elapsed_ms: row.get(6)?,
                error: row
                    .get::<_, Option<String>>(7)?
                    .map(|value| parse_json_column(7, &value))
                    .transpose()?,
            })
        })
        .map_err(|error| storage_error("OCR_ATTEMPT_QUERY_FAILED", error, true))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| storage_error("OCR_ATTEMPT_QUERY_FAILED", error, true))
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

fn explicitly_named_file_ids<'a>(
    question: &str,
    files: impl IntoIterator<Item = (Uuid, &'a str)>,
    scoped_file_ids: &HashSet<Uuid>,
) -> Option<HashSet<Uuid>> {
    static DOCUMENT_TITLE_PATTERN: OnceLock<Regex> = OnceLock::new();
    let pattern = DOCUMENT_TITLE_PATTERN
        .get_or_init(|| Regex::new(r"《([^《》]{1,260})》").expect("valid document title regex"));
    let titles = pattern
        .captures_iter(question)
        .filter_map(|captures| captures.get(1))
        .map(|capture| normalize_document_title(capture.as_str()))
        .filter(|title| !title.is_empty())
        .collect::<HashSet<_>>();
    if titles.is_empty() {
        return None;
    }

    let matches = files
        .into_iter()
        .filter(|(file_id, _)| scoped_file_ids.contains(file_id))
        .filter_map(|(file_id, display_name)| {
            let normalized_name = normalize_document_title(display_name);
            let normalized_stem = normalized_document_stem(&normalized_name);
            (titles.contains(&normalized_name) || titles.contains(normalized_stem))
                .then_some(file_id)
        })
        .collect::<HashSet<_>>();
    (!matches.is_empty()).then_some(matches)
}

fn question_requests_document_summary(question: &str) -> bool {
    ["概括", "总结", "主要内容", "主要在讲", "主要讲了什么"]
        .iter()
        .any(|cue| question.contains(cue))
}

fn load_structural_summary_evidence(
    connection: &Connection,
    files: &[FileRecord],
    file_ids: &HashSet<Uuid>,
    limit: usize,
) -> Result<Vec<(crate::EvidenceRef, AnswerSourceFile)>, AppError> {
    const STRUCTURAL_SUMMARY_FALLBACK_SQL: &str = "SELECT chunk_id, node_id, text, locator_json, image_asset_id, token_count, ordinal FROM (\
        SELECT c.chunk_id, c.node_id, c.text, c.locator_json, n.image_asset_id, c.token_count, c.ordinal, \
               ROW_NUMBER() OVER (ORDER BY c.ordinal) AS rn, COUNT(*) OVER () AS total \
        FROM chunks c JOIN document_nodes n ON n.node_id = c.node_id JOIN files f ON f.file_id = c.file_id \
        WHERE c.file_id = ?1 AND c.revision_id = f.current_revision_id) \
        WHERE rn = 1 OR rn = total OR rn = (total + 1) / 2 ORDER BY ordinal";
    const STRUCTURAL_SUMMARY_CANDIDATES_SQL: &str = "SELECT c.chunk_id, c.node_id, c.text, c.locator_json, n.image_asset_id, c.token_count, c.ordinal \
        FROM chunks c JOIN document_nodes n ON n.node_id = c.node_id JOIN files f ON f.file_id = c.file_id \
        WHERE c.file_id = ?1 AND c.revision_id = f.current_revision_id ORDER BY c.ordinal LIMIT 96";
    let mut evidence = Vec::new();
    let mut evidence_tokens = 0_u64;
    for file in files.iter().filter(|file| file_ids.contains(&file.file_id)) {
        let candidate_rows = {
            let mut statement = connection
                .prepare(STRUCTURAL_SUMMARY_CANDIDATES_SQL)
                .map_err(|error| storage_error("ASK_EVIDENCE_QUERY_FAILED", error, true))?;
            statement
                .query_map([file.file_id.to_string()], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, u64>(5)?,
                        row.get::<_, i64>(6)?,
                    ))
                })
                .map_err(|error| storage_error("ASK_EVIDENCE_QUERY_FAILED", error, true))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| storage_error("ASK_EVIDENCE_QUERY_FAILED", error, true))?
        };
        let mut rows = select_structural_summary_rows(
            candidate_rows,
            limit.saturating_sub(evidence.len()).min(3),
            Some(file.display_name.as_str()),
        );
        if rows.is_empty() {
            let mut statement = connection
                .prepare(STRUCTURAL_SUMMARY_FALLBACK_SQL)
                .map_err(|error| storage_error("ASK_EVIDENCE_QUERY_FAILED", error, true))?;
            rows = statement
                .query_map([file.file_id.to_string()], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, u64>(5)?,
                        row.get::<_, i64>(6)?,
                    ))
                })
                .map_err(|error| storage_error("ASK_EVIDENCE_QUERY_FAILED", error, true))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| storage_error("ASK_EVIDENCE_QUERY_FAILED", error, true))?;
        }
        for (chunk_id, node_id, quote, locator_json, image_asset_id, token_count, ordinal) in rows {
            if evidence.len() >= limit {
                return Ok(evidence);
            }
            if !evidence.is_empty() && evidence_tokens.saturating_add(token_count) > 2_400 {
                continue;
            }
            evidence_tokens = evidence_tokens.saturating_add(token_count);
            let (context_before, context_after) =
                fetch_neighbor_context(connection, &node_id, ordinal)?;
            evidence.push((
                crate::EvidenceRef {
                    evidence_id: Uuid::now_v7(),
                    file_id: file.file_id,
                    revision_id: file.current_revision_id.ok_or_else(|| {
                        AppError::new("ASK_EVIDENCE_INVALID", "目标文档缺少当前修订版本", false)
                    })?,
                    node_id: parse_uuid_value(&node_id)?,
                    chunk_id: parse_uuid_value(&chunk_id)?,
                    image_asset_id: image_asset_id
                        .as_deref()
                        .map(parse_uuid_value)
                        .transpose()?,
                    quote,
                    context_before,
                    context_after,
                    locator: serde_json::from_str::<SourceLocator>(&locator_json).map_err(
                        |error| AppError::new("ASK_EVIDENCE_INVALID", error.to_string(), false),
                    )?,
                    retrieval_score: 1.0,
                },
                AnswerSourceFile {
                    file_id: file.file_id,
                    display_name: file.display_name.clone(),
                    canonical_path: file.canonical_path.clone(),
                },
            ));
        }
    }
    Ok(evidence)
}

type StructuralSummaryRow = (String, String, String, String, Option<String>, u64, i64);

fn select_structural_summary_rows(
    rows: Vec<StructuralSummaryRow>,
    limit: usize,
    document_title: Option<&str>,
) -> Vec<StructuralSummaryRow> {
    if limit == 0 {
        return Vec::new();
    }
    let mut scored = rows
        .into_iter()
        .filter_map(|row| {
            let score = structural_summary_text_score(&row.2);
            (score > 0).then_some((score, row))
        })
        .collect::<Vec<_>>();
    let mut selected = Vec::new();
    let mut selected_chunk_ids = HashSet::new();
    if let Some(title) = document_title {
        if let Some(first_ordinal) = scored.iter().map(|(_, row)| row.6).min() {
            let title_candidate = scored
                .iter()
                .enumerate()
                .filter_map(|(index, (score, row))| {
                    if row.6.saturating_sub(first_ordinal) > 12 {
                        return None;
                    }
                    let title_score = structural_summary_title_overlap_score(&row.2, title);
                    (title_score > 0).then_some((index, title_score, *score, row.6))
                })
                .max_by(|left, right| {
                    left.1
                        .cmp(&right.1)
                        .then_with(|| left.2.cmp(&right.2))
                        .then_with(|| right.3.cmp(&left.3))
                });
            if let Some((index, _, _, _)) = title_candidate {
                let (_, row) = scored.remove(index);
                selected_chunk_ids.insert(row.0.clone());
                selected.push(row);
            }
        }
    }
    scored.sort_by(|(left_score, left), (right_score, right)| {
        right_score
            .cmp(left_score)
            .then_with(|| left.6.cmp(&right.6))
    });
    for (_, row) in scored {
        if selected.len() >= limit {
            break;
        }
        if selected_chunk_ids.insert(row.0.clone()) {
            selected.push(row);
        }
    }
    selected.sort_by_key(|row| row.6);
    selected
}

fn structural_summary_title_overlap_score(text: &str, document_title: &str) -> i64 {
    let normalized_text = normalize_document_title(text);
    let normalized_title = normalize_document_title(document_title);
    let title_stem = normalized_document_stem(&normalized_title);
    if title_stem.chars().count() < 4 {
        return 0;
    }
    if normalized_text.contains(title_stem) {
        return 1000;
    }

    let compact_text = retain_title_signal_characters(&normalized_text);
    let compact_title = retain_title_signal_characters(title_stem);
    let compact_len = compact_title.chars().count();
    if compact_len < 4 {
        return 0;
    }
    if compact_text.contains(&compact_title) {
        return 900;
    }

    let matched = compact_title
        .chars()
        .filter(|character| compact_text.contains(*character))
        .count();
    let required = ((compact_len as f64) * 0.72).ceil() as usize;
    if matched >= required && matched >= 8 {
        matched as i64
    } else {
        0
    }
}

fn retain_title_signal_characters(value: &str) -> String {
    value
        .chars()
        .filter(|character| {
            character.is_alphanumeric()
                || matches!(
                    *character as u32,
                    0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF
                )
        })
        .collect()
}
fn structural_summary_text_score(text: &str) -> i64 {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let char_count = normalized.chars().count();
    if char_count < 8 {
        return 0;
    }

    let mut han = 0_i64;
    let mut letters = 0_i64;
    let mut digits = 0_i64;
    let mut markup = 0_i64;
    let mut separators = 0_i64;
    for character in normalized.chars() {
        if matches!(
            character as u32,
            0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF
        ) {
            han += 1;
        } else if character.is_ascii_alphabetic() {
            letters += 1;
        } else if character.is_ascii_digit() {
            digits += 1;
        }
        if matches!(character, '\\' | '{' | '}' | '<' | '>' | ';') {
            markup += 1;
        }
        if matches!(character, '/' | '-' | '_' | ':' | ',' | '"' | '[' | ']') {
            separators += 1;
        }
    }

    let useful = han * 3 + letters + digits;
    if useful < 8 {
        return 0;
    }

    let lower = normalized.to_ascii_lowercase();
    let mut penalty = 0_i64;
    if char_count <= 16 {
        penalty += 80;
    }
    if markup * 5 > char_count as i64 {
        penalty += 180;
    }
    if separators * 4 > char_count as i64 && han < 8 {
        penalty += 80;
    }
    if lower.contains("\\rtf") || lower.contains("\\ansi") || lower.contains("\\fonttbl") {
        penalty += 420;
    }
    if normalized.chars().all(|character| {
        character.is_ascii_digit()
            || character.is_whitespace()
            || matches!(character, '/' | '.' | '-' | '_' | ':' | ')')
    }) {
        penalty += 220;
    }

    let mut score = useful.min(900);
    if char_count >= 40 {
        score += 40;
    }
    if normalized.contains('。')
        || normalized.contains('，')
        || normalized.contains('：')
        || normalized.contains("##")
    {
        score += 25;
    }
    (score - penalty).max(0)
}
fn normalize_document_title(value: &str) -> String {
    value
        .trim()
        .chars()
        .filter(|character| !character.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}

fn normalized_document_stem(normalized_name: &str) -> &str {
    normalized_name
        .rsplit_once('.')
        .map_or(normalized_name, |(stem, _)| stem)
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

fn join_search_channel<T>(
    handle: std::thread::ScopedJoinHandle<'_, Result<T, AppError>>,
) -> Result<T, AppError> {
    // 外层 join 捕获线程 panic，内层为通道自身错误，统一转成 AppError。
    handle
        .join()
        .map_err(|_| AppError::new("SEARCH_THREAD_PANIC", "搜索通道线程异常退出", false))?
}

/// 文件名/路径通道匹配：返回 (reason, score)，None 表示不匹配。
///
/// 覆盖真实查询的多种形态（按区分度从高到低）：
///   1. 完全一致：`周晨博-大模型开发.pdf` == `周晨博-大模型开发.pdf`
///   2. 规范化一致：去掉分隔符/标点后相等（`applogtxt` ↔ `app_log.txt`）
///   3. 文件名前缀：`周晨博` 命中 `周晨博-大模型开发.pdf`（比「包含」更强）
///   4. 文件名包含：`周晨博` 命中 `编译原理周晨博论文.docx`
///   5. 规范化包含：`applogtxt` 是 `app_log.txt` 规范化的子串
///   6. 词元全覆盖：查询每个 ≥2 字符词元都在文件名中出现
///      （`app_log txt` ↔ `app_log.txt`；分隔符差异不阻断匹配）
///   7. 路径包含：`软考` 命中目录路径含软考的文件（弱信号，score 最低）
///
/// query 已由调用方 trim + to_lowercase。name/path 已 to_lowercase。
fn filename_channel_match(name: &str, path: &str, query: &str) -> Option<(&'static str, f32)> {
    if query.is_empty() {
        return None;
    }
    if name == query {
        return Some(("filename", 1.0));
    }
    let normalized = |value: &str| {
        value
            .chars()
            .filter(|character| character.is_alphanumeric())
            .collect::<String>()
    };
    let normalized_query = normalized(query);
    let normalized_name = normalized(name);
    if !normalized_query.is_empty() && normalized_name == normalized_query {
        return Some(("filename", 0.95));
    }
    if name.starts_with(query) {
        return Some(("filename", 0.93));
    }
    if name.contains(query) {
        return Some(("filename", 0.9));
    }
    if !normalized_query.is_empty() && normalized_name.contains(&normalized_query) {
        return Some(("filename", 0.85));
    }
    let mut has_query_tokens = false;
    let mut all_tokens_covered = true;
    for token in query.split(|character: char| !character.is_alphanumeric()) {
        if token.chars().count() < 2 {
            continue;
        }
        has_query_tokens = true;
        if !name.contains(token) {
            all_tokens_covered = false;
            break;
        }
    }
    if has_query_tokens && all_tokens_covered {
        return Some(("filename", 0.8));
    }
    // 轻微错别字容错：查询与文件主干（去扩展名）的 Damerau-Levenshtein
    // 距离足够小且长度相近时，视为「用户打错字但指的就是这份文件」。
    // 覆盖真实拼写错误的常见形态：相邻字符交换（`kdsstep`↔`ksdstep`、
    // `YCLonfig2`↔`YLConfig2`）、单字符增删改。置信度低于精确包含匹配
    // （0.70），但仍高于 path 弱信号——错别字命中应排在纯语义/路径结果前。
    // 只在无任何更强匹配时启用（位于匹配链末尾）。
    const FILENAME_FUZZY_SCORE: f32 = 0.70;
    let query_stem = strip_extension_for_fuzzy(query);
    let name_stem = strip_extension_for_fuzzy(name);
    let query_len = query_stem.chars().count();
    let name_len = name_stem.chars().count();
    if query_len >= 3 && name_len >= 3 && (query_len as isize - name_len as isize).abs() <= 2 {
        let distance = damerau_levenshtein(&query_stem, &name_stem);
        // 距离上限随长度微增（约 1/4），最短也允许 1 处差异；
        // 长度相近的短名字（3-12 字符）只允许 1-2 处错误，避免误召回。
        let max_allowed = ((query_len.min(name_len) as f32 * 0.25).ceil() as usize).max(1);
        if distance <= max_allowed {
            return Some(("filename_fuzzy", FILENAME_FUZZY_SCORE));
        }
    }
    if path.contains(query) {
        return Some(("path", 0.65));
    }
    None
}

/// 剥一次扩展名（"ksdstep.ini" → "ksdstep"；无扩展名或"a.b"形态原样保留主名）。
/// 用于错别字容错的文件主干比较，避免扩展名干扰编辑距离。
fn strip_extension_for_fuzzy(value: &str) -> &str {
    match value.rsplit_once('.') {
        Some((stem, extension))
            if !stem.is_empty() && extension.chars().all(|ch| !ch.is_whitespace()) =>
        {
            stem
        }
        _ => value,
    }
}

/// Optimal String Alignment（Damerau-Levenshtein 变体）距离：
/// 相邻字符交换算 1 次编辑，覆盖中文/英文文件名最常见的错别字形态。
/// 纯字符级 DP，与具体内容无关。
fn damerau_levenshtein(a: &str, b: &str) -> usize {
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let n = a_chars.len();
    let m = b_chars.len();
    if n == 0 {
        return m;
    }
    if m == 0 {
        return n;
    }
    let mut dp = vec![vec![0usize; m + 1]; n + 1];
    for i in 0..=n {
        dp[i][0] = i;
    }
    for j in 0..=m {
        dp[0][j] = j;
    }
    for i in 1..=n {
        for j in 1..=m {
            let cost = usize::from(a_chars[i - 1] != b_chars[j - 1]);
            dp[i][j] = (dp[i - 1][j] + 1)
                .min(dp[i][j - 1] + 1)
                .min(dp[i - 1][j - 1] + cost);
            // 相邻字符交换：a[i-1]==b[j-2] 且 a[i-2]==b[j-1]。
            if i > 1
                && j > 1
                && a_chars[i - 1] == b_chars[j - 2]
                && a_chars[i - 2] == b_chars[j - 1]
            {
                dp[i][j] = dp[i][j].min(dp[i - 2][j - 2] + 1);
            }
        }
    }
    dp[n][m]
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
            "SELECT f.file_id, f.volume_id, f.canonical_path, f.display_name, f.extension, f.mime_type, f.size_bytes, f.fs_created_at, f.modified_at, f.windows_file_id, f.content_sha256, f.availability, f.current_revision_id, f.parse_status, f.first_seen_at, f.last_seen_at, c.revision_id, c.text, c.locator_json, bm25(chunks_fts), c.chunk_id, n.image_asset_id FROM chunks_fts JOIN chunks c ON c.chunk_id = chunks_fts.chunk_id JOIN document_nodes n ON n.node_id = c.node_id JOIN files f ON f.file_id = c.file_id WHERE chunks_fts MATCH ?1 AND f.current_revision_id = c.revision_id ORDER BY bm25(chunks_fts) LIMIT 500",
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
            Ok((
                file,
                revision_id,
                row.get::<_, String>(17)?,
                locator,
                rank,
                row.get::<_, String>(20)?,
                row.get::<_, Option<String>>(21)?,
            ))
        })
        .map_err(|error| storage_error("SEARCH_QUERY_FAILED", error, true))?;
    let mut hits = Vec::new();
    for row in mapped {
        let (file, revision_id, text, locator, rank, chunk_id, image_asset_id) =
            row.map_err(|error| storage_error("SEARCH_QUERY_FAILED", error, true))?;
        if !file_matches_scope(connection, &file, scope)? {
            continue;
        }
        let score = (1.0 / (1.0 + rank.abs())) as f32;
        let hit = RankedHit {
            file: file.clone(),
            chunk_id: Some(parse_uuid_value(&chunk_id)?),
            revision_id: Some(parse_uuid_value(&revision_id)?),
            image_asset_id: image_asset_id
                .as_deref()
                .map(parse_uuid_value)
                .transpose()?,
            snippet: text,
            locator: Some(locator),
            reason: "fulltext",
            channel_score: score,
        };
        hits.push(hit);
    }
    Ok(hits)
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
    let candidates = match search_semantic_usearch_candidates(connection, query, scoped_file_ids)? {
        Some(candidates) => candidates,
        None => search_semantic_exact_candidates(connection, query, scoped_file_ids)?,
    };

    // 逐候选查详情最多 5000 次 SQL（实测数百毫秒级）；改为一次 IN 查询取回
    // 全部详情再按候选顺序组装，语义检索热路径上的主要成本只剩向量搜索本身。
    let placeholders = std::iter::repeat_n("?", candidates.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT f.file_id, f.volume_id, f.canonical_path, f.display_name, f.extension, f.mime_type, f.size_bytes, f.fs_created_at, f.modified_at, f.windows_file_id, f.content_sha256, f.availability, f.current_revision_id, f.parse_status, f.first_seen_at, f.last_seen_at, c.revision_id, c.text, c.locator_json, c.chunk_id, n.image_asset_id FROM chunks c JOIN document_nodes n ON n.node_id = c.node_id JOIN files f ON f.file_id = c.file_id WHERE c.chunk_id IN ({placeholders}) AND f.current_revision_id = c.revision_id"
    );
    let mut values = Vec::<SqlValue>::with_capacity(candidates.len());
    for (_file_id, chunk_id, _score) in &candidates {
        values.push(SqlValue::Text(chunk_id.clone()));
    }
    let mut details = connection
        .prepare(&sql)
        .map_err(|error| storage_error("EMBEDDING_QUERY_FAILED", error, true))?;
    let rows = details
        .query_map(params_from_iter(values), |row| {
            let file = file_from_row(row)?;
            Ok((
                row.get::<_, String>(19)?,
                file,
                row.get::<_, String>(16)?,
                row.get::<_, String>(17)?,
                row.get::<_, String>(18)?,
                row.get::<_, Option<String>>(20)?,
            ))
        })
        .map_err(|error| storage_error("EMBEDDING_QUERY_FAILED", error, true))?;
    let mut detail_by_chunk =
        HashMap::<String, (crate::FileRecord, String, String, String, Option<String>)>::new();
    for row in rows {
        let (chunk_id, file, revision_id, text, locator_json, image_asset_id) =
            row.map_err(|error| storage_error("EMBEDDING_QUERY_FAILED", error, true))?;
        detail_by_chunk.insert(
            chunk_id,
            (file, revision_id, text, locator_json, image_asset_id),
        );
    }
    let mut hits = Vec::with_capacity(candidates.len());
    for (_file_id, chunk_id, score) in candidates {
        let Some((file, revision_id, text, locator_json, image_asset_id)) =
            detail_by_chunk.remove(&chunk_id)
        else {
            continue;
        };
        let locator = serde_json::from_str::<SourceLocator>(&locator_json)
            .map_err(|error| AppError::new("EMBEDDING_VECTOR_INVALID", error.to_string(), false))?;
        hits.push(RankedHit {
            file,
            chunk_id: Some(parse_uuid_value(&chunk_id)?),
            revision_id: Some(parse_uuid_value(&revision_id)?),
            image_asset_id: image_asset_id
                .as_deref()
                .map(parse_uuid_value)
                .transpose()?,
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
    let mut candidates = Vec::new();
    for row in mapped {
        let (chunk_id, file_id, dimension, vector_blob) =
            row.map_err(|error| storage_error("EMBEDDING_QUERY_FAILED", error, true))?;
        let file_id = parse_uuid_value(&file_id)?;
        if dimension as usize != query.vector.len() || !scoped_file_ids.contains(&file_id) {
            continue;
        }
        let similarity = dot_product_with_le_f32(query.vector, &vector_blob, dimension)?;
        let score = ((similarity + 1.0) / 2.0).clamp(0.0, 1.0);
        if score < crate::indexing::RAG_MIN_SEMANTIC_SCORE {
            continue;
        }
        candidates.push((file_id, chunk_id, score));
    }
    candidates.sort_by(|left, right| right.2.total_cmp(&left.2));
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
    // 小索引直接回退全量精确扫描：usearch HNSW 的 search 内部 beam 遍历深度
    // 与 count 成正比（index.hpp:3460），对 <5000 块的数据集（实测 1445 块）
    // 反而比全量点积慢 3-4 秒；全量点积在 5000 块内为毫秒级且无近似召回损失。
    const EXACT_SCAN_THRESHOLD: usize = 5000;
    if item_count < EXACT_SCAN_THRESHOLD {
        return Ok(None);
    }
    // usearch 的 search 内部 expansion = max(ef_search, wanted)（index.hpp:3460），
    // wanted 直接决定 beam 遍历深度。原实现 wanted=1000 实测每次 ~3-4s；
    // 现统一为 SEMANTIC_SEARCH_CANDIDATES（256）且 vector_index.rs 已把
    // ef_search 覆盖为同值，beam=256：速度提升约 4 倍，召回接近 ef=256 水平，
    // 返回 256 个候选对搜索/RAG 的 top-k 足够。
    let candidate_count = item_count.clamp(1, crate::vector_index::SEMANTIC_SEARCH_CANDIDATES);
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
    let mut candidates = Vec::new();
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
        if score < crate::indexing::RAG_MIN_SEMANTIC_SCORE {
            continue;
        }
        candidates.push((file_id, chunk_id, score));
    }
    candidates.sort_by(|left, right| right.2.total_cmp(&left.2));
    candidates.truncate(500);
    Ok(Some(candidates))
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

fn file_processing_disposition(extension: &str, mime_type: &str) -> FileProcessingDisposition {
    let extension = extension.to_ascii_lowercase();
    if mime_type.starts_with("image/") {
        return FileProcessingDisposition::ImageOcr;
    }
    if mime_type.starts_with("text/") {
        return FileProcessingDisposition::ReadOnlyText;
    }
    match extension.as_str() {
        "pdf" | "docx" | "docm" | "xlsx" | "xlsm" | "pptx" | "pptm" | "doc" | "xls" | "ppt" => {
            FileProcessingDisposition::ParseableContent
        }
        "zip" | "rar" | "7z" | "tar" | "gz" => FileProcessingDisposition::ArchiveManifest,
        "mp3" | "wav" | "flac" | "m4a" | "mp4" | "mkv" | "mov" | "avi" | "webm" => {
            FileProcessingDisposition::MediaMetadata
        }
        "exe" | "dll" | "msi" | "db" | "sqlite" | "sqlite3" => {
            FileProcessingDisposition::SafeMetadata
        }
        "ptqpaper" => FileProcessingDisposition::CapabilityMissing,
        _ => FileProcessingDisposition::Unknown,
    }
}

fn initial_parse_status(extension: &str, mime_type: &str) -> &'static str {
    if mime_type.starts_with("text/") || mime_type.starts_with("image/") {
        return "pending";
    }
    match extension.to_ascii_lowercase().as_str() {
        "pdf" | "docx" | "docm" | "xlsx" | "xlsm" | "pptx" | "pptm" | "csv" | "tsv" | "md"
        | "txt" | "html" | "htm" | "jpg" | "jpeg" | "png" | "tif" | "tiff" | "bmp" | "webp"
        | "doc" | "xls" | "ppt" | "zip" | "rs" | "py" | "js" | "jsx" | "mjs" | "cjs" | "ts"
        | "tsx" | "java" | "kt" | "kts" | "go" | "c" | "cc" | "cpp" | "h" | "hpp" | "cs" | "rb"
        | "php" | "swift" | "scala" | "sh" | "ps1" | "sql" | "json" | "yaml" | "yml" | "toml"
        | "xml" | "css" | "scss" | "vue" | "svelte" | "text" | "ini" | "iml" | "log" | "conf"
        | "cfg" | "properties" => "pending",
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
    let disposition = file_processing_disposition(&file.extension, &file.mime_type);
    let initial_parse_status = initial_parse_status(&file.extension, &file.mime_type);
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
                "INSERT OR IGNORE INTO files (file_id, canonical_path, path_key, name, extension, size_bytes, modified_at, discovered_at, availability, volume_id, display_name, mime_type, fs_created_at, windows_file_id, parse_status, first_seen_at, last_seen_at, processing_disposition, detected_mime_type) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'present', ?9, ?4, ?10, ?11, ?12, ?13, ?8, ?8, ?14, ?10)",
                params![file_id.to_string(), file.canonical_path, file.path_key, file.name, file.extension, file.size_bytes, file.modified_at.to_rfc3339(), now.to_rfc3339(), file.volume_id, file.mime_type, file.created_at.map(|value| value.to_rfc3339()), file.windows_file_id, initial_parse_status, disposition.as_str()],
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
            "INSERT INTO files (file_id, canonical_path, path_key, name, extension, size_bytes, modified_at, discovered_at, availability, volume_id, display_name, mime_type, fs_created_at, windows_file_id, current_revision_id, parse_status, first_seen_at, last_seen_at, processing_disposition, detected_mime_type) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'present', ?9, ?4, ?10, ?11, ?12, ?13, ?14, ?8, ?8, ?15, ?10) ON CONFLICT(file_id) DO UPDATE SET canonical_path = excluded.canonical_path, path_key = excluded.path_key, name = excluded.name, display_name = excluded.display_name, extension = excluded.extension, mime_type = excluded.mime_type, detected_mime_type = excluded.detected_mime_type, processing_disposition = excluded.processing_disposition, size_bytes = excluded.size_bytes, fs_created_at = excluded.fs_created_at, modified_at = excluded.modified_at, windows_file_id = excluded.windows_file_id, volume_id = excluded.volume_id, content_sha256 = CASE WHEN files.current_revision_id <> excluded.current_revision_id THEN NULL ELSE files.content_sha256 END, current_revision_id = excluded.current_revision_id, parse_status = CASE WHEN files.current_revision_id <> excluded.current_revision_id THEN excluded.parse_status ELSE files.parse_status END, availability = 'present', last_seen_at = excluded.last_seen_at",
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
                disposition.as_str(),
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

fn reconcile_root_memberships_batch(
    transaction: &Transaction<'_>,
    root_id: &Uuid,
    job_id: &Uuid,
    batch_size: usize,
) -> Result<usize, AppError> {
    let mut statement = transaction
        .prepare(
            "SELECT m.file_id FROM file_root_memberships m WHERE m.root_id = ?1 AND NOT EXISTS (SELECT 1 FROM scan_seen_memberships s WHERE s.job_id = ?2 AND s.root_id = ?1 AND s.file_id = m.file_id) LIMIT ?3",
        )
        .map_err(|error| storage_error("MEMBERSHIP_QUERY_FAILED", error, true))?;
    let existing = statement
        .query_map(
            params![root_id.to_string(), job_id.to_string(), batch_size as u64],
            |row| row.get::<_, String>(0),
        )
        .map_err(|error| storage_error("MEMBERSHIP_QUERY_FAILED", error, true))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| storage_error("MEMBERSHIP_QUERY_FAILED", error, true))?;
    drop(statement);
    let removed = existing.len();
    for file_id in existing {
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
    Ok(removed)
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
            "INSERT INTO inbox_events (inbox_id, dedupe_key, file_id, event_type, observed_at, previous_path, triage_status, summary, error_code, resolution_status) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, CASE WHEN ?4 IN ('parse_failed','ocr_required') OR ?9 IS NOT NULL THEN 'pending_retry' ELSE 'normal' END) ON CONFLICT(dedupe_key) DO UPDATE SET observed_at = excluded.observed_at, triage_status = excluded.triage_status, summary = excluded.summary, error_code = excluded.error_code, resolution_status = CASE WHEN inbox_events.resolution_status = 'abandoned' THEN 'abandoned' ELSE excluded.resolution_status END",
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
    let indexed_file_count: u64 = row.get(17)?;
    let indexable_file_count: u64 = row.get(18)?;
    let parsed_file_count: u64 = row.get(19)?;
    let embedded_file_count: u64 = row.get(20)?;
    let active_index_file_count: u64 = row.get(21)?;
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
        indexed_file_count,
        indexable_file_count,
        parsed_file_count,
        embedded_file_count,
        active_index_file_count,
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

/// document_profiles 查询行的原始列元组（不解析，供调用方宽松解析）。
type ProfileRow = (
    String,         // file_id
    String,         // revision_id
    String,         // title
    String,         // summary
    String,         // keywords_json
    String,         // entities_json
    Option<String>, // document_type
    Option<f64>,    // type_confidence
    String,         // section_titles_json
    Option<String>, // representative_text_hash
    String,         // updated_at
);

/// 行映射：原始列元组（rusqlite 闭包里只做类型转换，语义解析放外面）。
fn map_memory_entity_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryEntity> {
    let metadata_json: String = row.get(3)?;
    Ok(MemoryEntity {
        entity_id: parse_uuid_value(&row.get::<_, String>(0)?).map_err(storage_row_value)?,
        entity_type: row.get(1)?,
        canonical_name: row.get(2)?,
        metadata_json: serde_json::from_str(&metadata_json)
            .unwrap_or(serde_json::Value::Object(Default::default())),
        created_at: parse_datetime_value(&row.get::<_, String>(4)?).map_err(storage_row_value)?,
        updated_at: parse_datetime_value(&row.get::<_, String>(5)?).map_err(storage_row_value)?,
    })
}

fn map_memory_relation_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryRelation> {
    Ok(MemoryRelation {
        relation_id: parse_uuid_value(&row.get::<_, String>(0)?).map_err(storage_row_value)?,
        subject_type: MemoryTargetType::parse_storage(&row.get::<_, String>(1)?),
        subject_id: parse_uuid_value(&row.get::<_, String>(2)?).map_err(storage_row_value)?,
        predicate: row.get(3)?,
        object_type: MemoryTargetType::parse_storage(&row.get::<_, String>(4)?),
        object_id: parse_uuid_value(&row.get::<_, String>(5)?).map_err(storage_row_value)?,
        confidence: row.get::<_, f64>(6)? as f32,
        status: MemoryStatus::parse_storage(&row.get::<_, String>(7)?),
        source_type: MemorySource::parse_storage(&row.get::<_, String>(8)?),
        source_id: row.get(9)?,
        created_at: parse_datetime_value(&row.get::<_, String>(10)?).map_err(storage_row_value)?,
        updated_at: parse_datetime_value(&row.get::<_, String>(11)?).map_err(storage_row_value)?,
    })
}

fn map_memory_alias_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryAlias> {
    let last_used_at: Option<String> = row.get(8)?;
    Ok(MemoryAlias {
        alias_id: parse_uuid_value(&row.get::<_, String>(0)?).map_err(storage_row_value)?,
        alias: row.get(1)?,
        target_type: MemoryTargetType::parse_storage(&row.get::<_, String>(2)?),
        target_id: parse_uuid_value(&row.get::<_, String>(3)?).map_err(storage_row_value)?,
        confidence: row.get::<_, f64>(4)? as f32,
        status: MemoryStatus::parse_storage(&row.get::<_, String>(11)?),
        source_type: MemorySource::parse_storage(&row.get::<_, String>(5)?),
        source_id: row.get(6)?,
        hit_count: row.get::<_, i64>(7)? as u32,
        last_used_at: last_used_at
            .as_deref()
            .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
            .map(|value| value.with_timezone(&Utc)),
        created_at: parse_datetime_value(&row.get::<_, String>(9)?).map_err(storage_row_value)?,
        updated_at: parse_datetime_value(&row.get::<_, String>(10)?).map_err(storage_row_value)?,
    })
}

fn storage_row_value(error: AppError) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        0,
        rusqlite::types::Type::Text,
        Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            error.message,
        )),
    )
}

fn map_profile_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<(ProfileRow, Option<String>)> {
    Ok((
        (
            row.get(0)?,
            row.get(1)?,
            row.get(2)?,
            row.get(3)?,
            row.get(4)?,
            row.get(5)?,
            row.get(6)?,
            row.get(7)?,
            row.get(8)?,
            row.get(9)?,
            row.get(10)?,
        ),
        row.get(11).ok(),
    ))
}

/// 语义解析：原始列元组 → DocumentProfile。
fn parse_profile_row(raw: ProfileRow) -> Result<DocumentProfile, AppError> {
    let (
        file_id,
        revision_id,
        title,
        summary,
        keywords_json,
        entities_json,
        document_type,
        type_confidence,
        section_titles_json,
        representative_text_hash,
        updated_at,
    ) = raw;
    let invalid = |error: serde_json::Error| {
        AppError::new("DOCUMENT_PROFILE_INVALID", error.to_string(), false)
    };
    Ok(DocumentProfile {
        file_id: parse_uuid_value(&file_id)?,
        revision_id: parse_uuid_value(&revision_id)?,
        title,
        summary,
        keywords: serde_json::from_str(&keywords_json).map_err(invalid)?,
        entities: serde_json::from_str(&entities_json).map_err(invalid)?,
        document_type: document_type
            .as_deref()
            .and_then(DocumentType::parse_lenient),
        type_confidence: type_confidence.map(|value| value as f32),
        section_titles: serde_json::from_str(&section_titles_json).map_err(invalid)?,
        representative_text_hash,
        updated_at: parse_datetime_value(&updated_at)?,
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

/// 将 Option<Vec<String>> 序列化为落库用的 Option<JSON 字符串>（None → NULL）。
/// 评测多值字段（file/chunk/evidence/member）以 JSON 数组字符串存储。
fn json_list_or_null(values: Option<&Vec<String>>) -> Result<Option<String>, AppError> {
    values
        .map(|values| {
            serde_json::to_string(values).map_err(|error| {
                AppError::new("EVALUATION_SERIALIZE_FAILED", error.to_string(), false)
            })
        })
        .transpose()
}

/// 将数据库中的 Option<JSON 字符串> 解析为 Option<Vec<String>>（空/损坏回退 None）。
fn json_list_from_db(value: Option<String>) -> Option<Vec<String>> {
    value
        .as_deref()
        .and_then(|value| serde_json::from_str(value).ok())
}

/// 读取 evaluation_cases 一行（查询闭包专用；错误转为 rusqlite 错误）。
fn read_evaluation_case_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<EvaluationCaseRecord> {
    let metadata: String = row.get(15)?;
    let created_at = parse_datetime_column(&row.get::<_, String>(16)?, 16)?;
    Ok(EvaluationCaseRecord {
        case_id: row.get(0)?,
        feature_type: row.get(1)?,
        question_or_request: row.get(2)?,
        expected_source: row.get(3)?,
        expected_intent: row.get(4)?,
        expected_operation: row.get(5)?,
        expected_file_ids: json_list_from_db(row.get(6)?),
        expected_chunk_ids: json_list_from_db(row.get(7)?),
        expected_evidence_ids: json_list_from_db(row.get(8)?),
        expected_answer_shape: row.get(9)?,
        expected_relation_type: row.get(10)?,
        expected_collection_members: json_list_from_db(row.get(11)?),
        gold_reason: row.get(12)?,
        split: row.get(13)?,
        dataset_version: row.get(14)?,
        metadata: serde_json::from_str(&metadata).unwrap_or(serde_json::Value::Null),
        created_at,
    })
}

/// 读取 evaluation_runs 一行（查询闭包专用；错误转为 rusqlite 错误）。
fn read_evaluation_run_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<EvaluationRunRecord> {
    let metrics: String = row.get(8)?;
    let started_at = parse_datetime_column(&row.get::<_, String>(6)?, 6)?;
    let completed_at: Option<String> = row.get(7)?;
    let completed_at = completed_at
        .as_deref()
        .map(|value| parse_datetime_column(value, 7))
        .transpose()?;
    Ok(EvaluationRunRecord {
        run_id: row.get(0)?,
        dataset_version: row.get(1)?,
        code_revision: row.get(2)?,
        preset_id: row.get(3)?,
        model_ids: json_list_from_db(row.get(4)?),
        optimization_round: row.get::<_, i64>(5)? as u32,
        started_at,
        completed_at,
        metrics: serde_json::from_str(&metrics).unwrap_or(serde_json::Value::Null),
    })
}

/// 读取 evaluation_results 一行（查询闭包专用；错误转为 rusqlite 错误）。
fn read_evaluation_result_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<EvaluationResultRecord> {
    let metrics: String = row.get(12)?;
    let created_at = parse_datetime_column(&row.get::<_, String>(14)?, 14)?;
    Ok(EvaluationResultRecord {
        result_id: row.get(0)?,
        case_id: row.get(1)?,
        run_id: row.get(2)?,
        operation_id: row.get(3)?,
        pass_fail: row.get::<_, i64>(4)? != 0,
        error_category: row.get(5)?,
        diagnosis_reason: row.get(6)?,
        actual_source: row.get(7)?,
        actual_intent: row.get(8)?,
        actual_operation: row.get(9)?,
        actual_files: json_list_from_db(row.get(10)?),
        actual_evidence: json_list_from_db(row.get(11)?),
        metrics: serde_json::from_str(&metrics).unwrap_or(serde_json::Value::Null),
        latency_ms: row.get::<_, Option<i64>>(13)?.map(|value| value as u64),
        created_at,
    })
}

fn storage_error(code: &str, error: rusqlite::Error, retryable: bool) -> AppError {
    let (user_message, user_action) = map_storage_error(&error, code);
    let mut err = AppError::new(code, user_message, retryable);
    if let Some(action) = user_action {
        err.user_action = Some(action.into());
    }
    let mut details = serde_json::Map::new();
    details.insert(
        "technical".to_owned(),
        serde_json::Value::String(error.to_string()),
    );
    if let rusqlite::Error::SqliteFailure(sqlite, _) = &error {
        details.insert(
            "sqlite_primary_code".to_owned(),
            serde_json::Value::String(format!("{:?}", sqlite.code)),
        );
        details.insert(
            "sqlite_extended_code".to_owned(),
            serde_json::Value::from(sqlite.extended_code),
        );
    }
    err.details = Some(Box::new(serde_json::Value::Object(details)));
    err
}

fn is_transient_storage_error(error: &AppError) -> bool {
    error.details.as_deref().is_some_and(|details| {
        let technical = details
            .get("technical")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        let primary = details
            .get("sqlite_primary_code")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        technical.contains("database is locked")
            || technical.contains("database table is locked")
            || technical.contains("database is busy")
            || matches!(primary, "DatabaseBusy" | "DatabaseLocked")
    })
}

fn map_storage_error(error: &rusqlite::Error, _code: &str) -> (String, Option<&'static str>) {
    let text = error.to_string().to_lowercase();
    if text.contains("database is locked") || text.contains("database locked") {
        (
            "本地资料库暂时繁忙，请稍后重试".into(),
            Some("如果频繁出现，可尝试关闭其他同时访问资料的软件"),
        )
    } else if text.contains("disk i/o") || text.contains("disk full") || text.contains("no space") {
        (
            "磁盘读写出现问题".into(),
            Some("请检查磁盘空间是否充足，或是否有杀毒软件干扰了翻翻"),
        )
    } else if text.contains("readonly")
        || text.contains("read-only")
        || text.contains("permission denied")
    {
        (
            "资料库写入权限异常".into(),
            Some("请检查杀毒软件或系统权限设置，确保翻翻可以正常写入应用数据"),
        )
    } else if text.contains("no such table") || text.contains("no such column") {
        ("资料库结构需要升级，重启翻翻即可完成自动迁移".into(), None)
    } else if text.contains("unable to open") || text.contains("cannot open") {
        (
            "无法打开资料库文件".into(),
            Some("请确认翻翻应用数据目录未被移动或删除，重启后会自动修复"),
        )
    } else if text.contains("malformed")
        || text.contains("corrupt")
        || text.contains("not a database")
    {
        (
            "资料库文件损坏".into(),
            Some("请在设置中重建索引，源文件不会受到影响"),
        )
    } else if text.contains("busy") || text.contains("timeout") {
        ("资料库操作超时，请稍后重试".into(), None)
    } else {
        (
            "资料库操作失败，请重启翻翻后重试".into(),
            Some("如仍未解决，可在设置中导出诊断信息"),
        )
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
    use crate::OcrAttempt;

    #[test]
    fn explicit_document_titles_narrow_only_to_authorized_exact_names() {
        let target = Uuid::now_v7();
        let duplicate = Uuid::now_v7();
        let unrelated = Uuid::now_v7();
        let unauthorized = Uuid::now_v7();
        let files = [
            (target, "季度 复盘.MD"),
            (duplicate, "季度复盘.md"),
            (unrelated, "季度规划.md"),
            (unauthorized, "季度复盘.md"),
        ];
        let scoped = HashSet::from([target, duplicate, unrelated]);

        let matches = explicitly_named_file_ids(
            "请概括《季度复盘》并列出主要结论",
            files.iter().map(|(id, name)| (*id, *name)),
            &scoped,
        )
        .expect("an exact authorized title should narrow retrieval");

        assert_eq!(matches, HashSet::from([target, duplicate]));
        assert!(!matches.contains(&unauthorized));
    }

    #[test]
    fn explicit_document_title_requires_an_exact_match_before_narrowing() {
        let file_id = Uuid::now_v7();
        let files = [(file_id, "季度复盘.md")];
        let scoped = HashSet::from([file_id]);

        assert_eq!(
            explicitly_named_file_ids(
                "《季度复盘.md》讲了什么？",
                files.iter().map(|(id, name)| (*id, *name)),
                &scoped,
            ),
            Some(HashSet::from([file_id]))
        );
        assert_eq!(
            explicitly_named_file_ids(
                "《季度复》讲了什么？",
                files.iter().map(|(id, name)| (*id, *name)),
                &scoped,
            ),
            None,
            "partial titles must preserve the original retrieval scope"
        );
        assert_eq!(
            explicitly_named_file_ids(
                "这些季度资料讲了什么？",
                files.iter().map(|(id, name)| (*id, *name)),
                &scoped,
            ),
            None,
            "questions without an explicit document title keep broad retrieval"
        );
    }

    #[test]
    fn document_summary_detection_is_generic_and_does_not_depend_on_a_filename() {
        for question in [
            "请概括《任意文档.md》的主要内容",
            "总结一下这份资料",
            "这篇文章主要在讲什么？",
        ] {
            assert!(question_requests_document_summary(question));
        }
        assert!(!question_requests_document_summary(
            "《任意文档.md》是否提到了向量检索？"
        ));
    }

    #[test]
    fn structural_summary_score_rejects_markup_and_tiny_page_counters() {
        let natural =
            "本文介绍系统背景、核心模块、实验结果和后续改进方向，适合作为主要内容摘要的证据。";
        let rtf = r"{\rtf1 \ansi \fonttbl {\f0 Times New Roman;} \u9424 ?\uc1 \u9425 ?}";
        assert!(structural_summary_text_score(natural) > 0);
        assert_eq!(structural_summary_text_score("28/28"), 0);
        assert_eq!(structural_summary_text_score(rtf), 0);
    }

    #[test]
    fn structural_summary_rows_prefer_informative_chunks_without_expanding_scope() {
        fn row(text: &str, ordinal: i64) -> StructuralSummaryRow {
            (
                format!("chunk-{ordinal}"),
                format!("node-{ordinal}"),
                text.to_owned(),
                "{}".to_owned(),
                None,
                10,
                ordinal,
            )
        }

        let selected = select_structural_summary_rows(
            vec![
                row("希赛", 1),
                row("28/28", 2),
                row(
                    "本文围绕数据库系统工程师考试上午题，覆盖计算机组成、数据结构、网络安全和数据库基础等内容。",
                    3,
                ),
                row("最后总结实验结果、系统限制和未来改进方向。", 9),
            ],
            2,
            None,
        );
        assert_eq!(selected.len(), 2);
        assert_eq!(selected[0].6, 3);
        assert_eq!(selected[1].6, 9);
    }

    #[test]
    fn structural_summary_rows_keep_opening_title_when_it_matches_document_name() {
        fn row(text: &str, ordinal: i64) -> StructuralSummaryRow {
            (
                format!("chunk-{ordinal}"),
                format!("node-{ordinal}"),
                text.to_owned(),
                "{}".to_owned(),
                None,
                10,
                ordinal,
            )
        }

        let selected = select_structural_summary_rows(
            vec![
                row(
                    ")希赛 内部资料，禁止传播 2014年上半年数据库系统工程师考试上午真题（参考答案）",
                    1,
                ),
                row(
                    "3、海明码利用奇偶性检错和纠错，通过在n个数据位之间插入k个检验位。",
                    3,
                ),
                row(
                    "4、通常可以将计算机系统中执行一条指令的过程分为取指令，分析和执行指令3步。",
                    4,
                ),
                row("最后总结实验结果、系统限制和未来改进方向。", 40),
            ],
            3,
            Some("2014年上半年数据库系统工程师考试上午真题（参考答案）.pdf"),
        );

        assert_eq!(selected.len(), 3);
        assert_eq!(selected[0].6, 1);
        assert!(
            selected[0]
                .2
                .contains("2014年上半年数据库系统工程师考试上午真题")
        );
    }
    #[test]
    fn interactive_writes_are_not_starved_by_background_waiters() {
        let coordinator = Arc::new(WriteCoordinator::default());
        let held = coordinator.acquire(WritePriority::Background);
        let (sender, receiver) = std::sync::mpsc::channel();
        let (ready_sender, ready_receiver) = std::sync::mpsc::channel();

        let background = Arc::clone(&coordinator);
        let background_sender = sender.clone();
        let background_thread = thread::spawn(move || {
            ready_sender.send(()).expect("signal background ready");
            let _permit = background.acquire(WritePriority::Background);
            background_sender
                .send("background")
                .expect("send background");
        });
        ready_receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("background ready");

        let interactive = Arc::clone(&coordinator);
        let interactive_thread = thread::spawn(move || {
            let _permit = interactive.acquire(WritePriority::Interactive);
            sender.send("interactive").expect("send interactive");
        });
        while coordinator.state.lock().expect("state").interactive_waiters < 1 {
            thread::yield_now();
        }

        drop(held);
        assert_eq!(
            receiver
                .recv_timeout(Duration::from_secs(2))
                .expect("first writer"),
            "interactive"
        );
        assert_eq!(
            receiver
                .recv_timeout(Duration::from_secs(2))
                .expect("second writer"),
            "background"
        );
        interactive_thread.join().expect("interactive thread");
        background_thread.join().expect("background thread");
    }

    #[test]
    fn metadata_only_formats_never_enter_the_content_parser_queue() {
        for extension in ["exe", "dll", "msi", "mp4", "mp3", "7z", "unknown"] {
            assert_eq!(
                initial_parse_status(extension, "application/octet-stream"),
                "unsupported"
            );
        }
        for extension in [
            "pdf", "docx", "png", "zip", "rs", "py", "tsx", "text", "ini", "iml",
        ] {
            assert_eq!(initial_parse_status(extension, "text/plain"), "pending");
        }
    }

    #[test]
    fn scan_resume_keeps_committed_memberships_and_only_writes_missing_files() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = CatalogStore::open(directory.path().join("fanfan.db")).expect("open store");
        let root = store
            .upsert_root(&test_root_registration())
            .expect("insert root");
        let (job, _) = store
            .prepare_scan_job(&root.root_id, "resume")
            .expect("prepare scan");
        store
            .mark_scan_running(&root.root_id, &job.job_id)
            .expect("mark running");
        let first_path = directory.path().join("first.txt");
        let second_path = directory.path().join("second.txt");
        let first = test_discovered(&first_path, "first.txt");
        let second = test_discovered(&second_path, "second.txt");
        {
            let mut connection = store.connect().expect("connect");
            let transaction = connection.transaction().expect("transaction");
            let file_id = upsert_file(&transaction, &root.root_id, &first).expect("upsert first");
            transaction
                .execute(
                    "INSERT INTO scan_seen_memberships (job_id, root_id, file_id) VALUES (?1, ?2, ?3)",
                    params![job.job_id.to_string(), root.root_id.to_string(), file_id.to_string()],
                )
                .expect("checkpoint first");
            transaction.commit().expect("commit checkpoint");
        }

        let completed = store
            .commit_scan(
                &root.root_id,
                &job.job_id,
                &ScanOutcome {
                    files: vec![first, second],
                    ..ScanOutcome::default()
                },
            )
            .expect("resume scan");

        assert_eq!(completed.status, JobStatus::Succeeded);
        assert_eq!(store.list_files().expect("list files").len(), 2);
        let connection = store.connect().expect("connect checkpoint");
        let committed: u64 = connection
            .query_row(
                "SELECT committed_items FROM scan_checkpoints WHERE job_id = ?1",
                [job.job_id.to_string()],
                |row| row.get(0),
            )
            .expect("checkpoint row");
        assert_eq!(committed, 2);
    }

    #[test]
    fn deterministic_file_error_is_isolated_without_rolling_back_the_batch() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = CatalogStore::open(directory.path().join("fanfan.db")).expect("open store");
        let root = store
            .upsert_root(&test_root_registration())
            .expect("insert root");
        let (job, _) = store
            .prepare_scan_job(&root.root_id, "isolate")
            .expect("prepare scan");
        store
            .mark_scan_running(&root.root_id, &job.job_id)
            .expect("mark running");
        let mut invalid = test_discovered(&directory.path().join("invalid.txt"), "invalid.txt");
        invalid.size_bytes = u64::MAX;
        let valid = test_discovered(&directory.path().join("valid.txt"), "valid.txt");

        let completed = store
            .commit_scan(
                &root.root_id,
                &job.job_id,
                &ScanOutcome {
                    files: vec![invalid, valid],
                    ..ScanOutcome::default()
                },
            )
            .expect("partial scan");

        assert_eq!(completed.status, JobStatus::Partial);
        assert_eq!(store.list_files().expect("list files").len(), 1);
        let connection = store.connect().expect("connect attempts");
        let attempts: u64 = connection
            .query_row(
                "SELECT COUNT(*) FROM processing_attempts WHERE operation = 'scan_upsert' AND status = 'failed'",
                [],
                |row| row.get(0),
            )
            .expect("attempt count");
        assert_eq!(attempts, 1);
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

    fn test_discovered(path: &std::path::Path, name: &str) -> DiscoveredFile {
        let now = Utc::now();
        let extension = path
            .extension()
            .map(|value| value.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        DiscoveredFile {
            volume_id: "vol-test".into(),
            windows_file_id: None,
            canonical_path: path.to_string_lossy().into_owned(),
            path_key: path.to_string_lossy().to_ascii_lowercase(),
            name: name.into(),
            extension,
            mime_type: "text/plain".into(),
            size_bytes: 1,
            created_at: Some(now),
            modified_at: now,
            relative_path: name.into(),
        }
    }

    #[test]
    fn root_registration_is_idempotent() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = CatalogStore::open(directory.path().join("fanfan.db")).expect("open store");
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
        let store = CatalogStore::open(directory.path().join("fanfan.db")).expect("open store");
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
    fn index_activity_stats_report_authorized_current_index_content() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = CatalogStore::open(directory.path().join("fanfan.db")).expect("open store");
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
    fn coverage_and_root_counts_use_active_vector_index() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = CatalogStore::open(directory.path().join("fanfan.db")).expect("open store");
        let root = store
            .upsert_root(&test_root_registration())
            .expect("insert root");
        let generation_id = Uuid::now_v7();
        let now = Utc::now().to_rfc3339();
        let connection = store.connect().expect("connect");
        connection
            .execute(
                "INSERT INTO index_generations (generation_id, model_artifact_id, dimension, metric, quantization, index_path, status, item_count, coverage, created_at, activated_at) VALUES (?1, 'embedding-test', 2, 'cosine', 'f32', 'active.usearch', 'active', 1, 0.5, ?2, ?2)",
                params![generation_id.to_string(), now],
            )
            .expect("insert active generation");

        let mut seeded_chunks = Vec::new();
        for (name, active_key) in [("active.pdf", Some(1_i64)), ("embedded-only.pdf", None)] {
            let file_id = Uuid::now_v7();
            let revision_id = Uuid::now_v7();
            let node_id = Uuid::now_v7();
            let chunk_id = Uuid::now_v7();
            let path = format!("C:\\Users\\Test\\Documents\\{name}");
            let path_key = path.to_ascii_lowercase();
            connection
                .execute(
                    "INSERT INTO files (file_id, canonical_path, path_key, name, extension, size_bytes, modified_at, discovered_at, availability, volume_id, display_name, mime_type, current_revision_id, parse_status, first_seen_at, last_seen_at, processing_disposition) VALUES (?1, ?2, ?3, ?4, 'pdf', 256, ?5, ?5, 'present', 'vol-test', ?4, 'application/pdf', ?6, 'parsed', ?5, ?5, 'parseable_content')",
                    params![file_id.to_string(), path, path_key, name, now, revision_id.to_string()],
                )
                .expect("insert file");
            connection
                .execute(
                    "INSERT INTO file_revisions (revision_id, file_id, size_bytes, fs_modified_at, metadata_fingerprint, created_at, parse_status) VALUES (?1, ?2, 256, ?3, ?4, ?3, 'parsed')",
                    params![revision_id.to_string(), file_id.to_string(), now, format!("256:{name}")],
                )
                .expect("insert revision");
            connection
                .execute(
                    "INSERT INTO file_root_memberships (file_id, root_id, relative_path, is_primary) VALUES (?1, ?2, ?3, 1)",
                    params![file_id.to_string(), root.root_id.to_string(), name],
                )
                .expect("insert membership");
            connection
                .execute(
                    "INSERT INTO document_nodes (node_id, revision_id, ordinal, node_type, locator_json, heading_path_json, text) VALUES (?1, ?2, 0, 'paragraph', '{}', '[]', ?3)",
                    params![node_id.to_string(), revision_id.to_string(), name],
                )
                .expect("insert node");
            connection
                .execute(
                    "INSERT INTO chunks (chunk_id, file_id, revision_id, node_id, ordinal, text, normalized_text, token_count, content_hash, language, locator_json) VALUES (?1, ?2, ?3, ?4, 0, ?5, ?5, 4, ?1, 'zh', '{}')",
                    params![chunk_id.to_string(), file_id.to_string(), revision_id.to_string(), node_id.to_string(), name],
                )
                .expect("insert chunk");
            connection
                .execute(
                    "INSERT INTO chunk_embeddings (chunk_id, model_artifact_id, file_id, revision_id, dimension, vector_blob, created_at) VALUES (?1, 'embedding-test', ?2, ?3, 2, X'00000000', ?4)",
                    params![chunk_id.to_string(), file_id.to_string(), revision_id.to_string(), now],
                )
                .expect("insert embedding");
            if let Some(vector_key) = active_key {
                connection
                    .execute(
                        "INSERT INTO vector_index_keys (generation_id, vector_key, chunk_id, file_id, revision_id) VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![generation_id.to_string(), vector_key, chunk_id.to_string(), file_id.to_string(), revision_id.to_string()],
                    )
                    .expect("insert active vector key");
            }
            seeded_chunks.push(chunk_id);
        }

        let unsupported_id = Uuid::now_v7();
        connection
            .execute(
                "INSERT INTO files (file_id, canonical_path, path_key, name, extension, size_bytes, modified_at, discovered_at, availability, volume_id, display_name, mime_type, parse_status, first_seen_at, last_seen_at, processing_disposition) VALUES (?1, 'C:\\Users\\Test\\Documents\\tool.exe', 'c:\\users\\test\\documents\\tool.exe', 'tool.exe', 'exe', 128, ?2, ?2, 'present', 'vol-test', 'tool.exe', 'application/x-msdownload', 'unsupported', ?2, ?2, 'capability_missing')",
                params![unsupported_id.to_string(), now],
            )
            .expect("insert unsupported file");
        connection
            .execute(
                "INSERT INTO file_root_memberships (file_id, root_id, relative_path, is_primary) VALUES (?1, ?2, 'tool.exe', 1)",
                params![unsupported_id.to_string(), root.root_id.to_string()],
            )
            .expect("insert unsupported membership");
        drop(connection);

        let coverage = store
            .processing_coverage_snapshot()
            .expect("coverage snapshot");
        assert_eq!(coverage.fts_chunks, 2);
        assert_eq!(coverage.embedding_chunks, 2);
        assert_eq!(coverage.active_vector_keys, 1);
        assert_eq!(coverage.embedding_coverage, 1.0);
        assert_eq!(coverage.vector_coverage, 0.5);

        let maintenance = store.maintenance_snapshot().expect("maintenance");
        assert_eq!(maintenance.indexable_files, 2);
        assert_eq!(maintenance.parsed_files, 2);
        assert_eq!(maintenance.embedded_files, 2);
        assert_eq!(maintenance.active_index_files, 1);
        assert_eq!(maintenance.indexed_files, 1);
        assert_eq!(maintenance.searchable_chunks, 2);
        assert_eq!(maintenance.embedded_chunks, 2);
        assert_eq!(maintenance.active_vector_keys, 1);

        let root = store.list_roots().expect("roots").remove(0);
        assert_eq!(root.indexable_file_count, 2);
        assert_eq!(root.parsed_file_count, 2);
        assert_eq!(root.embedded_file_count, 2);
        assert_eq!(root.active_index_file_count, 1);
        assert_eq!(root.indexed_file_count, 1);
    }
    /// 造一个「已解析 + 已嵌入」的文件：files/revisions/root 归属/节点/chunk/embedding 全链。
    /// 单 chunk → 文档向量即该 chunk 向量（mean_normalized_vector 归一化后 cosine 不变）。
    /// 返回 (file_id, revision_id)。
    fn seed_embedded_file(
        connection: &Connection,
        root_id: &Uuid,
        name: &str,
        size_bytes: u64,
        vector: &[f32],
        text: &str,
    ) -> (Uuid, Uuid) {
        let file_id = Uuid::now_v7();
        let revision_id = Uuid::now_v7();
        let node_id = Uuid::now_v7();
        let chunk_id = Uuid::now_v7();
        let now = Utc::now().to_rfc3339();
        let path = format!("C:\\Users\\Test\\Documents\\{name}");
        let path_key = path.to_ascii_lowercase();
        connection
            .execute(
                "INSERT INTO files (file_id, canonical_path, path_key, name, extension, size_bytes, modified_at, discovered_at, availability, volume_id, display_name, mime_type, current_revision_id, parse_status, first_seen_at, last_seen_at) VALUES (?1, ?2, ?3, ?4, 'pdf', ?5, ?6, ?6, 'present', 'vol-test', ?4, 'application/pdf', ?7, 'parsed', ?6, ?6)",
                params![file_id.to_string(), path, path_key, name, size_bytes, now, revision_id.to_string()],
            )
            .expect("insert file");
        connection
            .execute(
                "INSERT INTO file_revisions (revision_id, file_id, size_bytes, fs_modified_at, metadata_fingerprint, created_at, parse_status) VALUES (?1, ?2, ?3, ?4, ?5, ?4, 'parsed')",
                params![revision_id.to_string(), file_id.to_string(), size_bytes, now, format!("{size_bytes}:{name}")],
            )
            .expect("insert revision");
        connection
            .execute(
                "INSERT INTO file_root_memberships (file_id, root_id, relative_path, is_primary) VALUES (?1, ?2, ?3, 1)",
                params![file_id.to_string(), root_id.to_string(), name],
            )
            .expect("insert membership");
        connection
            .execute(
                "INSERT INTO document_nodes (node_id, revision_id, ordinal, node_type, locator_json, heading_path_json, text) VALUES (?1, ?2, 0, 'ocr_line', ?3, '[]', ?4)",
                params![node_id.to_string(), revision_id.to_string(), serde_json::json!({"page_no": 1}).to_string(), text],
            )
            .expect("insert node");
        connection
            .execute(
                "INSERT INTO chunks (chunk_id, file_id, revision_id, node_id, ordinal, text, normalized_text, token_count, content_hash, language, locator_json) VALUES (?1, ?2, ?3, ?4, 0, ?5, ?5, 4, ?1, 'zh', '{}')",
                params![chunk_id.to_string(), file_id.to_string(), revision_id.to_string(), node_id.to_string(), text],
            )
            .expect("insert chunk");
        connection
            .execute(
                "INSERT INTO chunk_embeddings (chunk_id, model_artifact_id, file_id, revision_id, dimension, vector_blob, created_at) VALUES (?1, 'embedding-test', ?2, ?3, ?4, ?5, ?6)",
                params![chunk_id.to_string(), file_id.to_string(), revision_id.to_string(), vector.len() as u32, encode_vector(vector), now],
            )
            .expect("insert embedding");
        (file_id, revision_id)
    }

    /// 关系分析走星形种子扩展：1 种子 + 3 语义成员（cos≈0.88/0.83/0.80，成员互 <0.78）
    /// + 1「摘要」名小文件（与种子 cos≈0.85 → contains 边）+ 1 远文件（cos≈0.75 不组）。
    ///
    /// 应只产生种子→成员的星形边，成员间无直连边，摘要方向正确，远文件无边。
    #[test]
    fn semantic_relations_use_star_seed_expansion() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = CatalogStore::open(directory.path().join("fanfan.db")).expect("open store");
        let root = store
            .upsert_root(&test_root_registration())
            .expect("insert root");
        // 6 维全正卦限 → 同一语义桶（种子与成员同桶才比较）；成员彼此夹角 >38.7°（互 cos<0.78）。
        let seed_vector = [1.0, 0.3, 0.3, 0.3, 0.3, 0.3];
        let member_a = [1.0, 1.1, 0.3, 0.3, 0.3, 0.3]; // cos(种子)≈0.876
        let member_b = [1.0, 0.3, 1.3, 0.3, 0.3, 0.3]; // ≈0.832
        let member_c = [1.0, 0.3, 0.3, 1.45, 0.3, 0.3]; // ≈0.801
        let summary_d = [1.0, 0.3, 0.3, 0.3, 1.2, 0.3]; // ≈0.854（摘要名 + 小文件）
        // 远文件：增强第 6 维（与各成员主方向正交），与种子 ≈0.753、与成员全部 <0.78
        let far_e = [1.0, 0.3, 0.3, 0.3, 0.3, 1.7]; // ≈0.753（不组）
        let connection = store.connect().expect("connect");
        let seed = seed_embedded_file(
            &connection,
            &root.root_id,
            "季度财务报告.pdf",
            2_000,
            &seed_vector,
            "财务季度报告全文内容",
        );
        let a = seed_embedded_file(
            &connection,
            &root.root_id,
            "财务数据明细.pdf",
            1_800,
            &member_a,
            "财务数据明细内容",
        );
        let b = seed_embedded_file(
            &connection,
            &root.root_id,
            "年度预算说明.pdf",
            1_900,
            &member_b,
            "年度预算编制说明内容",
        );
        let c = seed_embedded_file(
            &connection,
            &root.root_id,
            "审计意见书.pdf",
            1_700,
            &member_c,
            "审计意见与结论内容",
        );
        let d = seed_embedded_file(
            &connection,
            &root.root_id,
            "财务报告摘要.pdf",
            120,
            &summary_d,
            "季度财务报告要点摘要",
        );
        let e = seed_embedded_file(
            &connection,
            &root.root_id,
            "员工手册.pdf",
            3_000,
            &far_e,
            "员工手册内容",
        );
        drop(connection);

        let (semantic_pairs, contains_pairs) = store
            .refresh_semantic_file_relations("embedding-test", 100)
            .expect("refresh semantic relations");
        assert_eq!((semantic_pairs, contains_pairs), (3, 1));

        let connection = store.connect().expect("connect");
        let rows = {
            let mut statement = connection
                .prepare(
                    "SELECT left_file_id, right_file_id, relation_type, confidence FROM file_relations WHERE model_version = 'embedding-test'",
                )
                .expect("prepare relation query");
            statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, f64>(3)?,
                    ))
                })
                .expect("query relations")
                .collect::<Result<Vec<_>, _>>()
                .expect("collect relations")
        };
        drop(connection);
        assert_eq!(rows.len(), 4, "星形边总数应为 4（种子→A/B/C/D）");
        let seed_id = seed.0.to_string();
        let member_ids = [a.0, b.0, c.0, d.0].map(|id| id.to_string());
        let far_id = e.0.to_string();
        for (left, right, relation_type, confidence) in &rows {
            // 每条边都以种子为一端；成员间无直连边；远文件不参与
            assert!(
                left == &seed_id || right == &seed_id,
                "非星形边 {left}-{right}"
            );
            assert!(
                !member_ids.contains(left) || !member_ids.contains(right),
                "成员间直连边 {left}-{right}"
            );
            assert!(left != &far_id && right != &far_id, "远文件不该有边");
            assert!(
                (0.75..=1.0).contains(confidence),
                "相似度置信度异常 {confidence}"
            );
            assert!(
                relation_type == "semantic_related" || relation_type == "contains_or_summarizes",
                "意外的边类型 {relation_type}"
            );
        }
        let semantic = rows
            .iter()
            .filter(|(_, _, relation_type, _)| relation_type == "semantic_related")
            .count();
        let contains = rows
            .iter()
            .filter(|(_, _, relation_type, _)| relation_type == "contains_or_summarizes")
            .count();
        assert_eq!(semantic, 3);
        assert_eq!(contains, 1);
        // 摘要边：D（摘要名+小文件）与种子，confidence≈0.854，方向为「摘要可能是源文件的摘要」
        let (_, _, _, confidence) = rows
            .iter()
            .find(|(_, _, relation_type, _)| relation_type == "contains_or_summarizes")
            .expect("contains edge");
        assert!(
            (confidence - 0.854).abs() < 0.01,
            "contains 置信度 {confidence}"
        );
    }

    /// 集合建议种子扩展 + 消费 + 修剪全流程：两簇各 4 文件（簇 1 全正卦限、簇 2 全负卦限，
    /// 不同语义桶 → 跨簇不组）。首批 2 条建议、成员互斥、种子身份回传；重复刷新不重现
    /// （成员已消费）；confirm 后仍不重现；prune 剪成员 → 幂等键重算 + inbox 清理；
    /// 剪到 <2 → 整条作废。
    #[test]
    fn collection_suggestions_seed_expansion_consumption_and_prune_flow() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = CatalogStore::open(directory.path().join("fanfan.db")).expect("open store");
        let root = store
            .upsert_root(&test_root_registration())
            .expect("insert root");
        // 簇 1（财务）：全正卦限桶；簇 2（人力）：全负卦限桶（桶不同，跨簇不组）
        let connection = store.connect().expect("connect");
        let s1 = seed_embedded_file(
            &connection,
            &root.root_id,
            "财务季度报告.pdf",
            2_000,
            &[1.0, 0.3, 0.3, 0.3, 0.3, 0.3],
            "财务季度报告全文内容",
        );
        seed_embedded_file(
            &connection,
            &root.root_id,
            "财务数据明细.pdf",
            1_800,
            &[1.0, 1.1, 0.3, 0.3, 0.3, 0.3],
            "财务数据明细内容",
        );
        seed_embedded_file(
            &connection,
            &root.root_id,
            "年度预算说明.pdf",
            1_900,
            &[1.0, 0.3, 1.3, 0.3, 0.3, 0.3],
            "年度预算编制说明内容",
        );
        seed_embedded_file(
            &connection,
            &root.root_id,
            "审计意见书.pdf",
            1_700,
            &[1.0, 0.3, 0.3, 1.45, 0.3, 0.3],
            "审计意见与结论内容",
        );
        let s2 = seed_embedded_file(
            &connection,
            &root.root_id,
            "招聘制度汇编.pdf",
            2_000,
            &[-1.0, -0.3, -0.3, -0.3, -0.3, -0.3],
            "招聘制度汇编内容",
        );
        seed_embedded_file(
            &connection,
            &root.root_id,
            "薪酬管理办法.pdf",
            1_800,
            &[-1.0, -1.1, -0.3, -0.3, -0.3, -0.3],
            "薪酬管理办法内容",
        );
        seed_embedded_file(
            &connection,
            &root.root_id,
            "考勤管理制度.pdf",
            1_900,
            &[-1.0, -0.3, -1.3, -0.3, -0.3, -0.3],
            "考勤管理制度内容",
        );
        seed_embedded_file(
            &connection,
            &root.root_id,
            "绩效评估细则.pdf",
            1_700,
            &[-1.0, -0.3, -0.3, -1.45, -0.3, -0.3],
            "绩效评估细则内容",
        );
        drop(connection);

        // 首批：两簇各成一组，created=2，种子身份回传与 suggestion_ids 一一对应
        let first = store
            .refresh_collection_suggestions("embedding-test", 500)
            .expect("first refresh");
        assert_eq!(first.topic_groups, 2);
        assert_eq!(first.created_suggestions, 2);
        assert_eq!(first.suggestion_ids.len(), 2);
        assert_eq!(first.seed_file_id_by_suggestion.len(), 2);
        let mut seeds = first
            .suggestion_ids
            .iter()
            .map(|id| first.seed_file_id_by_suggestion[id])
            .collect::<Vec<_>>();
        let mut expected_seeds = [s1.0, s2.0];
        seeds.sort();
        expected_seeds.sort();
        assert_eq!(seeds, expected_seeds, "种子身份回传应恰好是两簇的核文件");

        // 成员互斥：8 文件无重叠；每组 3 个成员（种子是组的核，不进成员表）
        let connection = store.connect().expect("connect");
        let member_count: u64 = connection
            .query_row(
                "SELECT COUNT(*) FROM collection_suggested_members",
                [],
                |row| row.get(0),
            )
            .expect("member count");
        let distinct_count: u64 = connection
            .query_row(
                "SELECT COUNT(DISTINCT file_id) FROM collection_suggested_members",
                [],
                |row| row.get(0),
            )
            .expect("distinct member count");
        let per_group = {
            let mut statement = connection
                .prepare("SELECT COUNT(*) FROM collection_suggested_members GROUP BY suggestion_id")
                .expect("prepare group counts");
            statement
                .query_map([], |row| row.get::<_, u64>(0))
                .expect("query group counts")
                .collect::<Result<Vec<_>, _>>()
                .expect("collect group counts")
        };
        assert_eq!(member_count, 6, "8 文件 - 2 种子 = 6 个组成员");
        assert_eq!(distinct_count, 6, "无文件同时属于两个建议（互斥）");
        assert_eq!(
            per_group,
            vec![3, 3],
            "每组成员数为 3（种子是组的核不进成员表）"
        );
        drop(connection);

        // 重复刷新：档案已最新且成员全消费 → 无新建议（幂等不重现）
        let second = store
            .refresh_collection_suggestions("embedding-test", 500)
            .expect("second refresh");
        assert_eq!(second.topic_groups, 0);
        assert_eq!(second.created_suggestions, 0);
        assert!(second.suggestion_ids.is_empty());
        assert!(second.seed_file_id_by_suggestion.is_empty());

        // confirm 一条后再次刷新：已确认建议的成员仍在 consumed 集 → 仍不重现
        store
            .confirm_collection_suggestion(&first.suggestion_ids[0])
            .expect("confirm first suggestion");
        let third = store
            .refresh_collection_suggestions("embedding-test", 500)
            .expect("third refresh");
        assert_eq!(third.topic_groups, 0);
        assert_eq!(third.created_suggestions, 0);

        // prune 流：对「种子为 s2 的建议」剪掉 1 个成员 → 存活，幂等键重算，inbox 只删被剪成员的。
        // 注意 suggestion_ids 的顺序取决于 UUID v7 随机位，必须按 seed map 定位建议，
        // 并从成员表取实际成员，不能假设「第一个建议 = 簇 1」。
        let suggestion_two = first
            .suggestion_ids
            .iter()
            .copied()
            .find(|id| first.seed_file_id_by_suggestion[id] == s2.0)
            .expect("种子为 s2 的建议");
        let connection = store.connect().expect("connect");
        let group_members = {
            let mut statement = connection
                .prepare(
                    "SELECT file_id FROM collection_suggested_members WHERE suggestion_id = ?1 ORDER BY file_id",
                )
                .expect("prepare member query");
            statement
                .query_map([suggestion_two.to_string()], |row| row.get::<_, String>(0))
                .expect("query members")
                .collect::<Result<Vec<_>, _>>()
                .expect("collect members")
        };
        assert_eq!(group_members.len(), 3, "s2 组的成员数应为 3");
        let prune_one = Uuid::parse_str(&group_members[0]).expect("member uuid");
        let prune_rest = group_members[1..]
            .iter()
            .map(|value| Uuid::parse_str(value).expect("member uuid"))
            .collect::<Vec<_>>();
        let key_before: String = connection
            .query_row(
                "SELECT idempotency_key FROM collection_suggestions WHERE suggestion_id = ?1",
                [suggestion_two.to_string()],
                |row| row.get(0),
            )
            .expect("idempotency key before prune");
        let pruned_event_before: u64 = connection
            .query_row(
                "SELECT COUNT(*) FROM inbox_events WHERE dedupe_key = ?1",
                [format!(
                    "collection_suggestion:{suggestion_two}:{prune_one}"
                )],
                |row| row.get(0),
            )
            .expect("pruned member inbox before prune");
        drop(connection);
        assert_eq!(pruned_event_before, 1);

        let survived = store
            .prune_collection_suggestion_members(&suggestion_two, &[prune_one])
            .expect("prune one member");
        assert!(survived, "剩 2 个成员应存活");
        let connection = store.connect().expect("connect");
        let key_after: String = connection
            .query_row(
                "SELECT idempotency_key FROM collection_suggestions WHERE suggestion_id = ?1",
                [suggestion_two.to_string()],
                |row| row.get(0),
            )
            .expect("idempotency key after prune");
        let pruned_event_after: u64 = connection
            .query_row(
                "SELECT COUNT(*) FROM inbox_events WHERE dedupe_key = ?1",
                [format!(
                    "collection_suggestion:{suggestion_two}:{prune_one}"
                )],
                |row| row.get(0),
            )
            .expect("pruned member inbox after prune");
        let kept_event_after: u64 = connection
            .query_row(
                "SELECT COUNT(*) FROM inbox_events WHERE dedupe_key = ?1",
                [format!(
                    "collection_suggestion:{suggestion_two}:{}",
                    prune_rest[0]
                )],
                |row| row.get(0),
            )
            .expect("kept member inbox after prune");
        drop(connection);
        assert_ne!(key_before, key_after, "修剪后幂等键必须重算");
        assert_eq!(pruned_event_after, 0, "被剪成员的 inbox 事件删除");
        assert_eq!(kept_event_after, 1, "存活成员的事件保留");

        // 剩余 2 个成员再剪光 → 整条作废（建议行+成员行+inbox 全清）
        let survived = store
            .prune_collection_suggestion_members(&suggestion_two, &prune_rest)
            .expect("prune to zero");
        assert!(!survived, "成员不足 2 个应作废整条建议");
        let connection = store.connect().expect("connect");
        let suggestion_rows: u64 = connection
            .query_row(
                "SELECT COUNT(*) FROM collection_suggestions WHERE suggestion_id = ?1",
                [suggestion_two.to_string()],
                |row| row.get(0),
            )
            .expect("suggestion rows");
        let member_rows: u64 = connection
            .query_row(
                "SELECT COUNT(*) FROM collection_suggested_members WHERE suggestion_id = ?1",
                [suggestion_two.to_string()],
                |row| row.get(0),
            )
            .expect("member rows");
        let inbox_rows: u64 = connection
            .query_row(
                "SELECT COUNT(*) FROM inbox_events WHERE dedupe_key LIKE ?1",
                [format!("collection_suggestion:{suggestion_two}:%")],
                |row| row.get(0),
            )
            .expect("inbox rows");
        drop(connection);
        assert_eq!(suggestion_rows, 0);
        assert_eq!(member_rows, 0);
        assert_eq!(inbox_rows, 0);
    }

    #[test]
    fn degradation_state_persists_and_changes_only_one_level_per_checkpoint() {
        let directory = tempfile::tempdir().expect("tempdir");
        let database_path = directory.path().join("fanfan.db");
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
        let store = CatalogStore::open(directory.path().join("fanfan.db")).expect("open store");
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
        let store = CatalogStore::open(directory.path().join("fanfan.db")).expect("open store");

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
                (13, "knowledge_spaces_removed".to_owned()),
                (14, "application_settings".to_owned()),
                (15, "relation_evidence_versions".to_owned()),
                (16, "stable_keyset_paging_and_scan_checkpoints".to_owned()),
                (17, "inbox_triage_resolution_split".to_owned()),
                (18, "remove_knowledge_spaces".to_owned()),
                (19, "local_ai_runtime_and_media_transcripts".to_owned()),
                (20, "observable_ocr_attempts".to_owned()),
                (21, "recoverable_processing_and_scan_checkpoints".to_owned()),
                (22, "node_traces".to_owned()),
                (23, "chunk_neighbor_context_index".to_owned()),
                (24, "relation_groups".to_owned()),
                (25, "encrypted_disposition_consistency".to_owned()),
                (26, "ask_session_context".to_owned()),
                (27, "document_profiles_classifier_columns".to_owned()),
                (28, "memory_layer".to_owned()),
                (29, "ask_session_clarification".to_owned()),
                (30, "memory_alias_status".to_owned()),
                (31, "purge_deleted_session_residue".to_owned()),
                (32, "operation_trace_infrastructure".to_owned()),
                (33, "evaluation_loop_persistence".to_owned()),
                (34, "image_ocr_before_vision".to_owned()),
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
        let database_path = directory.path().join("fanfan.db");
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
    fn delete_ask_session_purges_all_related_rows() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = CatalogStore::open(directory.path().join("fanfan.db")).expect("open store");
        let session_id = uuid::Uuid::now_v7();
        let connection = store.connect().expect("connect");
        let session_key = session_id.to_string();
        let now = chrono::Utc::now().to_rfc3339();
        // 构造一个包含消息、上下文与节点 trace 的会话
        connection
            .execute(
                "INSERT INTO ask_sessions (session_id, scope_json, created_at, updated_at) \
                 VALUES (?1, '{}', ?2, ?2)",
                [&session_key, &now],
            )
            .expect("insert session");
        connection
            .execute(
                "INSERT INTO ask_messages (message_id, session_id, role, content, created_at) \
                 VALUES (?1, ?2, 'user', '我的简历里写了什么', ?3)",
                [
                    uuid::Uuid::now_v7().to_string(),
                    session_key.clone(),
                    now.clone(),
                ],
            )
            .expect("insert message");
        connection
            .execute(
                "INSERT INTO ask_session_context (session_id, updated_at) VALUES (?1, ?2)",
                [&session_key, &now],
            )
            .expect("insert context");
        connection
            .execute(
                "INSERT INTO node_traces (trace_id, flow, node, correlation_id, session_id, \
                 input_json, output_json, status, created_at) \
                 VALUES (?1, 'ask', 'retrieval', ?2, ?3, '{}', '{}', 'ok', ?4)",
                [
                    uuid::Uuid::now_v7().to_string(),
                    uuid::Uuid::now_v7().to_string(),
                    session_key.clone(),
                    now.clone(),
                ],
            )
            .expect("insert trace");
        drop(connection);

        store
            .delete_ask_session(&session_id)
            .expect("delete session");

        // 四张表必须全部清空：会话、消息、上下文、节点 trace
        let connection = store.connect().expect("reconnect");
        for (label, sql) in [
            ("ask_sessions", "SELECT COUNT(*) FROM ask_sessions"),
            ("ask_messages", "SELECT COUNT(*) FROM ask_messages"),
            (
                "ask_session_context",
                "SELECT COUNT(*) FROM ask_session_context",
            ),
            (
                "node_traces",
                &format!("SELECT COUNT(*) FROM node_traces WHERE session_id = '{session_key}'"),
            ),
        ] {
            let count: i64 = connection.query_row(sql, [], |row| row.get(0)).unwrap();
            assert_eq!(count, 0, "{label} 应随会话删除同步清空");
        }
        // 不存在的会话删除应报错且不误删其他数据
        assert_eq!(
            store.delete_ask_session(&session_id).unwrap_err().code,
            "ASK_SESSION_NOT_FOUND"
        );
    }

    #[test]
    fn ask_session_context_persists_round_trip_and_clears() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = CatalogStore::open(directory.path().join("fanfan.db")).expect("open store");
        let session_id = uuid::Uuid::now_v7();
        let file_a = uuid::Uuid::now_v7();
        let file_b = uuid::Uuid::now_v7();

        // 无记录 → None
        assert!(
            store
                .get_ask_session_context(session_id)
                .expect("read context")
                .is_none()
        );

        let now = chrono::Utc::now();
        let context = crate::AskSessionContext {
            session_id: Some(session_id),
            active_file_id: Some(file_a),
            active_file_ids: vec![file_a, file_b],
            active_document_type: Some(crate::DocumentType::Resume),
            active_entity_id: Some(file_b),
            active_collection_id: None,
            last_referenced_file_ids: vec![file_b],
            last_intent: Some("document_qa".to_owned()),
            pending_clarification_reference: Some("第一份".to_owned()),
            updated_at: Some(now),
        };
        store
            .update_ask_session_context(session_id, &context)
            .expect("write context");

        let loaded = store
            .get_ask_session_context(session_id)
            .expect("read context")
            .expect("context exists");
        assert_eq!(loaded.active_file_id, Some(file_a));
        assert_eq!(loaded.active_file_ids, vec![file_a, file_b]);
        assert_eq!(
            loaded.active_document_type,
            Some(crate::DocumentType::Resume)
        );
        assert_eq!(loaded.active_entity_id, Some(file_b));
        assert_eq!(loaded.last_referenced_file_ids, vec![file_b]);
        assert_eq!(loaded.last_intent.as_deref(), Some("document_qa"));
        assert_eq!(
            loaded.pending_clarification_reference.as_deref(),
            Some("第一份")
        );
        assert!(loaded.updated_at.is_some());

        // upsert：第二次写入覆盖而非新增
        let mut second = context.clone();
        second.active_file_id = Some(file_b);
        second.last_intent = Some("document_summary".to_owned());
        store
            .update_ask_session_context(session_id, &second)
            .expect("update context");
        let reloaded = store
            .get_ask_session_context(session_id)
            .expect("read context")
            .expect("context exists");
        assert_eq!(reloaded.active_file_id, Some(file_b));
        assert_eq!(reloaded.last_intent.as_deref(), Some("document_summary"));

        // clear 后回到无记录
        store
            .clear_ask_session_context(session_id)
            .expect("clear context");
        assert!(
            store
                .get_ask_session_context(session_id)
                .expect("read context")
                .is_none()
        );
    }

    #[test]
    fn document_profile_classifier_columns_round_trip() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = CatalogStore::open(directory.path().join("fanfan.db")).expect("open store");
        let file_id = Uuid::now_v7();
        let revision_id = Uuid::now_v7();
        let now = Utc::now().to_rfc3339();

        // 直接落一行画像（模拟 organizing.rs 写入基础列，migration 27 后扩展列为默认值）
        {
            let connection = store.connect().expect("connect");
            connection
                .execute(
                    "INSERT INTO files (file_id, canonical_path, path_key, name, extension, size_bytes, modified_at, discovered_at, availability) \
                     VALUES (?1, ?1 || ':path', ?1 || ':path', 'resume.pdf', 'pdf', 1024, ?2, ?2, 'present')",
                    params![file_id.to_string(), now],
                )
                .expect("insert file");
            connection
                .execute(
                    "INSERT INTO file_revisions (revision_id, file_id, size_bytes, fs_modified_at, metadata_fingerprint, created_at) \
                     VALUES (?1, ?2, 1024, ?3, 'fp', ?3)",
                    params![revision_id.to_string(), file_id.to_string(), now],
                )
                .expect("insert revision");
            connection
                .execute(
                    "INSERT INTO document_profiles (file_id, revision_id, title, summary, keywords_json, entities_json, embedding_model_id, dimension, vector_blob, candidate_bucket, algorithm_version, created_at, updated_at) \
                     VALUES (?1, ?2, '我的简历', '软件工程师简历', '[]', '[]', 'emb', 0, X'', 'bucket', 'v1', ?3, ?3)",
                    params![file_id.to_string(), revision_id.to_string(), now],
                )
                .expect("insert profile");
        }

        // 扩展列初始为默认值
        let initial = store
            .get_document_profile(file_id)
            .expect("read profile")
            .expect("profile exists");
        assert_eq!(initial.title, "我的简历");
        assert_eq!(initial.document_type, None);
        assert_eq!(initial.section_titles, Vec::<String>::new());

        // 分类器写入扩展列后回读
        let updated = DocumentProfile {
            document_type: Some(DocumentType::Resume),
            type_confidence: Some(0.97),
            section_titles: vec!["项目经历".to_owned(), "教育背景".to_owned()],
            representative_text_hash: Some("sha256:abc123".to_owned()),
            ..initial
        };
        assert!(
            store
                .update_document_profile_classifier(&updated)
                .expect("update classifier columns")
        );

        let loaded = store
            .get_document_profile(file_id)
            .expect("read profile")
            .expect("profile exists");
        assert_eq!(loaded.document_type, Some(DocumentType::Resume));
        assert_eq!(loaded.type_confidence, Some(0.97));
        assert_eq!(
            loaded.section_titles,
            vec!["项目经历".to_owned(), "教育背景".to_owned()]
        );
        assert_eq!(
            loaded.representative_text_hash.as_deref(),
            Some("sha256:abc123")
        );

        // 不存在画像行的更新是不带副作用的 no-op
        let missing = DocumentProfile {
            file_id: Uuid::now_v7(),
            ..updated.clone()
        };
        assert!(
            !store
                .update_document_profile_classifier(&missing)
                .expect("update missing profile is no-op")
        );
        assert!(
            store
                .get_document_profile(missing.file_id)
                .expect("read missing profile")
                .is_none()
        );
    }

    /// 画像构建测试用种子：parsed 文件 + 多个带章节路径的节点 + 若干 chunk
    /// （chunk 数与嵌入数可分别控制，用于验证「全量嵌入」门槛）。
    #[allow(clippy::too_many_arguments)]
    fn seed_profile_file(
        connection: &Connection,
        root_id: &Uuid,
        name: &str,
        revision_id: Uuid,
        heading_paths: &[&str],
        chunk_texts: &[&str],
        embedded_count: usize,
        model_artifact_id: &str,
    ) -> (Uuid, Uuid) {
        let file_id = Uuid::now_v7();
        let now = Utc::now().to_rfc3339();
        let path = format!("C:\\Users\\Test\\Documents\\{name}");
        let path_key = path.to_ascii_lowercase();
        connection
            .execute(
                "INSERT INTO files (file_id, canonical_path, path_key, name, extension, size_bytes, modified_at, discovered_at, availability, volume_id, display_name, mime_type, current_revision_id, parse_status, first_seen_at, last_seen_at) VALUES (?1, ?2, ?3, ?4, 'pdf', 1024, ?5, ?5, 'present', 'vol-test', ?4, 'application/pdf', ?6, 'parsed', ?5, ?5)",
                params![file_id.to_string(), path, path_key, name, now, revision_id.to_string()],
            )
            .expect("insert file");
        connection
            .execute(
                "INSERT INTO file_revisions (revision_id, file_id, size_bytes, fs_modified_at, metadata_fingerprint, created_at, parse_status) VALUES (?1, ?2, 1024, ?3, ?4, ?3, 'parsed')",
                params![revision_id.to_string(), file_id.to_string(), now, format!("{name}:{revision_id}")],
            )
            .expect("insert revision");
        connection
            .execute(
                "INSERT INTO file_root_memberships (file_id, root_id, relative_path, is_primary) VALUES (?1, ?2, ?3, 1)",
                params![file_id.to_string(), root_id.to_string(), name],
            )
            .expect("insert membership");
        // 节点数 = max(标题路径数, chunk 数)：保证每个 chunk 都有对应节点
        let node_count = heading_paths.len().max(chunk_texts.len());
        for ordinal in 0..node_count {
            let node_id = Uuid::now_v7();
            connection
                .execute(
                    "INSERT INTO document_nodes (node_id, revision_id, ordinal, node_type, locator_json, heading_path_json, text) VALUES (?1, ?2, ?3, 'text', ?4, ?5, ?6)",
                    params![
                        node_id.to_string(),
                        revision_id.to_string(),
                        ordinal as u64,
                        serde_json::json!({"page_no": 1}).to_string(),
                        heading_paths.get(ordinal).copied().unwrap_or("[]"),
                        chunk_texts.get(ordinal).copied().unwrap_or(""),
                    ],
                )
                .expect("insert node");
        }
        for (ordinal, text) in chunk_texts.iter().enumerate() {
            let chunk_id = Uuid::now_v7();
            connection
                .execute(
                    "INSERT INTO chunks (chunk_id, file_id, revision_id, node_id, ordinal, text, normalized_text, token_count, content_hash, language, locator_json) VALUES (?1, ?2, ?3, (SELECT node_id FROM document_nodes WHERE revision_id = ?3 AND ordinal = ?4 LIMIT 1), ?4, ?5, ?5, 4, ?1, 'zh', '{}')",
                    params![chunk_id.to_string(), file_id.to_string(), revision_id.to_string(), ordinal as u64, text],
                )
                .expect("insert chunk");
            if ordinal < embedded_count {
                let vector = [0.1 + ordinal as f32 * 0.05, 0.2, 0.3, 0.4, 0.5, 0.6];
                connection
                    .execute(
                        "INSERT INTO chunk_embeddings (chunk_id, model_artifact_id, file_id, revision_id, dimension, vector_blob, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                        params![
                            chunk_id.to_string(),
                            model_artifact_id,
                            file_id.to_string(),
                            revision_id.to_string(),
                            vector.len() as u32,
                            encode_vector(&vector),
                            now,
                        ],
                    )
                    .expect("insert embedding");
            }
        }
        (file_id, revision_id)
    }

    #[test]
    fn document_profile_builder_builds_profile_from_parsed_embedded_file() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = CatalogStore::open(directory.path().join("fanfan.db")).expect("open store");
        let root = store
            .upsert_root(&test_root_registration())
            .expect("insert root");
        let (file_id, revision_id) = {
            let connection = store.connect().expect("connect");
            seed_profile_file(
                &connection,
                &root.root_id,
                "大模型开发工程师-周晨.pdf",
                Uuid::now_v7(),
                &[
                    r#"[]"#,
                    r#"["教育背景"]"#,
                    r#"["项目经历","LangGraph 多智能体"]"#,
                    r#"["项目经历"]"#,
                ],
                &[
                    "软件工程师简历概览",
                    "北京大学 计算机专业",
                    "LangGraph 多智能体项目介绍",
                    "项目经历总结",
                ],
                4,
                "embedding-test",
            )
        };

        let result = store
            .refresh_document_profiles("embedding-test", 200)
            .expect("refresh profiles");
        assert_eq!(result.profiled_files, 1);
        assert_eq!(result.skipped_files, 0);

        let profile = store
            .get_document_profile(file_id)
            .expect("read profile")
            .expect("profile exists");
        assert_eq!(profile.revision_id, revision_id);
        assert_eq!(profile.title, "大模型开发工程师-周晨.pdf");
        // section_titles：叶子标题、文档顺序、去重
        assert_eq!(
            profile.section_titles,
            vec!["教育背景", "LangGraph 多智能体", "项目经历"]
        );
        // summary = 首个 chunk 压缩文本
        assert_eq!(profile.summary, "软件工程师简历概览");
        // 代表性文本哈希非空且稳定（代表内容含 title + sections + head/mid/tail）
        let hash = profile.representative_text_hash.expect("hash present");
        assert!(hash.starts_with("0x") || hash.chars().all(|ch| ch.is_ascii_hexdigit()));
        assert!(!profile.keywords.is_empty());
        // 扩展列初始为空（分类器 Step 2 写入）
        assert_eq!(profile.document_type, None);
    }

    #[test]
    fn document_profile_builder_skips_files_without_or_partial_embeddings() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = CatalogStore::open(directory.path().join("fanfan.db")).expect("open store");
        let root = store
            .upsert_root(&test_root_registration())
            .expect("insert root");
        let (no_embed_file, partial_file) = {
            let connection = store.connect().expect("connect");
            // 无任何嵌入 → 不进入候选（门槛：至少存在一条当前模型嵌入）
            let no_embed = seed_profile_file(
                &connection,
                &root.root_id,
                "无嵌入.pdf",
                Uuid::now_v7(),
                &[r#"["教育背景"]"#],
                &["只有文本没有向量"],
                0,
                "embedding-test",
            );
            // 部分嵌入（2/3 chunk 有向量）→ 不进入候选（门槛：全量嵌入完成）
            let partial = seed_profile_file(
                &connection,
                &root.root_id,
                "部分嵌入.pdf",
                Uuid::now_v7(),
                &[r#"["项目经历"]"#],
                &["第一段", "第二段", "第三段"],
                2,
                "embedding-test",
            );
            (no_embed, partial)
        };

        let result = store
            .refresh_document_profiles("embedding-test", 200)
            .expect("refresh profiles");
        // 两个文件都被门槛排除：不建画像、不计数为失败（静默等待嵌入收敛）
        assert_eq!(result.profiled_files, 0);
        assert!(
            store
                .get_document_profile(no_embed_file.0)
                .expect("read profile")
                .is_none()
        );
        assert!(
            store
                .get_document_profile(partial_file.0)
                .expect("read profile")
                .is_none()
        );
    }

    #[test]
    fn document_profile_lifecycle_tracks_revision_and_invalidates_stale() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = CatalogStore::open(directory.path().join("fanfan.db")).expect("open store");
        let root = store
            .upsert_root(&test_root_registration())
            .expect("insert root");
        let file_id = {
            let connection = store.connect().expect("connect");
            let (file_id, _) = seed_profile_file(
                &connection,
                &root.root_id,
                "我的简历.pdf",
                Uuid::now_v7(),
                &[r#"["项目经历"]"#],
                &["第一版简历内容"],
                1,
                "embedding-test",
            );
            file_id
        };
        store
            .refresh_document_profiles("embedding-test", 200)
            .expect("build v1 profile");
        let listed = store
            .list_document_profiles(None, 200)
            .expect("list profiles");
        assert_eq!(listed.len(), 1);

        // 文件更新：只新增 file_revisions 行 + 节点/chunk/嵌入（真实索引流程中
        // 文件行不重复插入），files.current_revision_id 切换到新 revision。
        let new_revision = {
            let connection = store.connect().expect("connect");
            let new_revision = Uuid::now_v7();
            let now = Utc::now().to_rfc3339();
            connection
                .execute(
                    "INSERT INTO file_revisions (revision_id, file_id, size_bytes, fs_modified_at, metadata_fingerprint, created_at, parse_status) VALUES (?1, ?2, 1024, ?3, ?4, ?3, 'parsed')",
                    params![
                        new_revision.to_string(),
                        file_id.to_string(),
                        now,
                        format!("我的简历.pdf:v2")
                    ],
                )
                .expect("insert new revision");
            for (ordinal, (heading, text)) in [
                (r#"["项目经历"]"#, "第二版简历内容"),
                (r#"["技能"]"#, "LangGraph 技能描述"),
            ]
            .iter()
            .enumerate()
            {
                let node_id = Uuid::now_v7();
                connection
                    .execute(
                        "INSERT INTO document_nodes (node_id, revision_id, ordinal, node_type, locator_json, heading_path_json, text) VALUES (?1, ?2, ?3, 'text', '{}', ?4, ?5)",
                        params![node_id.to_string(), new_revision.to_string(), ordinal as u64, heading, text],
                    )
                    .expect("insert new-revision node");
                let chunk_id = Uuid::now_v7();
                connection
                    .execute(
                        "INSERT INTO chunks (chunk_id, file_id, revision_id, node_id, ordinal, text, normalized_text, token_count, content_hash, language, locator_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, 4, ?1, 'zh', '{}')",
                        params![
                            chunk_id.to_string(),
                            file_id.to_string(),
                            new_revision.to_string(),
                            node_id.to_string(),
                            ordinal as u64,
                            text
                        ],
                    )
                    .expect("insert new-revision chunk");
                let vector = [0.1, 0.2, 0.3, 0.4, 0.5, 0.6];
                connection
                    .execute(
                        "INSERT INTO chunk_embeddings (chunk_id, model_artifact_id, file_id, revision_id, dimension, vector_blob, created_at) VALUES (?1, 'embedding-test', ?2, ?3, ?4, ?5, ?6)",
                        params![
                            chunk_id.to_string(),
                            file_id.to_string(),
                            new_revision.to_string(),
                            vector.len() as u32,
                            encode_vector(&vector),
                            now
                        ],
                    )
                    .expect("insert new-revision embedding");
            }
            connection
                .execute(
                    "UPDATE files SET current_revision_id = ?1 WHERE file_id = ?2",
                    params![new_revision.to_string(), file_id.to_string()],
                )
                .expect("switch current revision");
            new_revision
        };

        // 画像仍是旧 revision：list_document_profiles 必须过滤掉（stale 不参与定位）
        let listed = store
            .list_document_profiles(None, 200)
            .expect("list stale-filtered profiles");
        assert!(listed.is_empty(), "旧 revision 的画像不得出现在定位候选里");

        // 生产链重建 → 画像绑定新 revision 且内容更新
        let result = store
            .refresh_document_profiles("embedding-test", 200)
            .expect("rebuild profile");
        assert_eq!(result.profiled_files, 1);
        let profile = store
            .get_document_profile(file_id)
            .expect("read profile")
            .expect("profile exists");
        assert_eq!(profile.revision_id, new_revision);
        assert_eq!(profile.section_titles, vec!["项目经历", "技能"]);
        let listed = store
            .list_document_profiles(None, 200)
            .expect("list profiles after rebuild");
        assert_eq!(listed.len(), 1);
    }

    #[test]
    fn document_profile_builder_preserves_classifier_columns() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = CatalogStore::open(directory.path().join("fanfan.db")).expect("open store");
        let root = store
            .upsert_root(&test_root_registration())
            .expect("insert root");
        let file_id = {
            let connection = store.connect().expect("connect");
            let (file_id, _) = seed_profile_file(
                &connection,
                &root.root_id,
                "我的简历.pdf",
                Uuid::now_v7(),
                &[r#"["项目经历"]"#],
                &["第一版简历内容"],
                1,
                "embedding-test",
            );
            file_id
        };
        store
            .refresh_document_profiles("embedding-test", 200)
            .expect("build profile");
        // 分类器写入 document_type（Step 2 的生产调用；这里模拟写入）
        let mut classified = store
            .get_document_profile(file_id)
            .expect("read profile")
            .expect("profile exists");
        classified.document_type = Some(DocumentType::Resume);
        classified.type_confidence = Some(0.96);
        store
            .update_document_profile_classifier(&classified)
            .expect("classify");

        // 再次构建画像：基础列更新，分类器列必须原样保留（生产链不覆写）
        store
            .refresh_document_profiles("embedding-test", 200)
            .expect("rebuild profile");
        let profile = store
            .get_document_profile(file_id)
            .expect("read profile")
            .expect("profile exists");
        assert_eq!(profile.document_type, Some(DocumentType::Resume));
        assert_eq!(profile.type_confidence, Some(0.96));
    }

    #[test]
    fn revision_change_resets_classifier_columns_and_pending_list_serves_classifier() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = CatalogStore::open(directory.path().join("fanfan.db")).expect("open store");
        let root = store
            .upsert_root(&test_root_registration())
            .expect("insert root");
        let file_id = {
            let connection = store.connect().expect("connect");
            let (file_id, _) = seed_profile_file(
                &connection,
                &root.root_id,
                "我的简历.pdf",
                Uuid::now_v7(),
                &[r#"["项目经历"]"#],
                &["第一版简历内容"],
                1,
                "embedding-test",
            );
            file_id
        };
        store
            .refresh_document_profiles("embedding-test", 200)
            .expect("build v1 profile");

        // 分类器写入类型 + 置信度
        let mut classified = store
            .get_document_profile(file_id)
            .expect("read profile")
            .expect("profile exists");
        classified.document_type = Some(DocumentType::Resume);
        classified.type_confidence = Some(0.96);
        store
            .update_document_profile_classifier(&classified)
            .expect("classify");
        // 已分类的画像不再出现在待分类列表里
        assert_eq!(
            store
                .list_profiles_needing_classification(200)
                .expect("pending list")
                .len(),
            0
        );
        // profile_vector 能读回画像已存的文档级向量（分类器与原型比对用）；
        // 存的是均值归一化后的向量（与 semantic_cluster_v3 口径一致）：
        // 方向与 [0.1..0.6] 相同、模长 ≈ 1
        let vector = store
            .profile_vector(&file_id)
            .expect("read profile vector")
            .expect("vector exists");
        let magnitude = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
        assert!(
            (magnitude - 1.0).abs() < 1e-5,
            "profile vector must be normalized"
        );
        for (actual, expected) in vector.iter().zip([0.1_f32, 0.2, 0.3, 0.4, 0.5, 0.6]) {
            let scale = *actual / expected;
            assert!(
                (scale - vector[0] / 0.1).abs() < 1e-5,
                "profile vector must be collinear with chunk mean"
            );
        }

        // 文件更新（新 revision 只加 revision 行 + 内容，files.current_revision_id 切换）
        let new_revision = {
            let connection = store.connect().expect("connect");
            let new_revision = Uuid::now_v7();
            let now = Utc::now().to_rfc3339();
            connection
                .execute(
                    "INSERT INTO file_revisions (revision_id, file_id, size_bytes, fs_modified_at, metadata_fingerprint, created_at, parse_status) VALUES (?1, ?2, 1024, ?3, ?4, ?3, 'parsed')",
                    params![
                        new_revision.to_string(),
                        file_id.to_string(),
                        now,
                        format!("我的简历.pdf:v2")
                    ],
                )
                .expect("insert new revision");
            let node_id = Uuid::now_v7();
            connection
                .execute(
                    "INSERT INTO document_nodes (node_id, revision_id, ordinal, node_type, locator_json, heading_path_json, text) VALUES (?1, ?2, ?3, 'text', '{}', ?4, ?5)",
                    params![node_id.to_string(), new_revision.to_string(), 0_u64, r#"["项目经历"]"#, "第二版简历内容"],
                )
                .expect("insert new-revision node");
            let chunk_id = Uuid::now_v7();
            connection
                .execute(
                    "INSERT INTO chunks (chunk_id, file_id, revision_id, node_id, ordinal, text, normalized_text, token_count, content_hash, language, locator_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, 4, ?1, 'zh', '{}')",
                    params![
                        chunk_id.to_string(),
                        file_id.to_string(),
                        new_revision.to_string(),
                        node_id.to_string(),
                        0_u64,
                        "第二版简历内容"
                    ],
                )
                .expect("insert new-revision chunk");
            let vector = [0.1, 0.2, 0.3, 0.4, 0.5, 0.6];
            connection
                .execute(
                    "INSERT INTO chunk_embeddings (chunk_id, model_artifact_id, file_id, revision_id, dimension, vector_blob, created_at) VALUES (?1, 'embedding-test', ?2, ?3, ?4, ?5, ?6)",
                    params![
                        chunk_id.to_string(),
                        file_id.to_string(),
                        new_revision.to_string(),
                        vector.len() as u32,
                        encode_vector(&vector),
                        now
                    ],
                )
                .expect("insert new-revision embedding");
            connection
                .execute(
                    "UPDATE files SET current_revision_id = ?1 WHERE file_id = ?2",
                    params![new_revision.to_string(), file_id.to_string()],
                )
                .expect("switch current revision");
            new_revision
        };

        // 重建画像：revision 变化 → 分类器列必须清空（旧类型绝不带到新版本），
        // 且待分类列表重新包含该画像（等分类器重判）。
        store
            .refresh_document_profiles("embedding-test", 200)
            .expect("rebuild profile");
        let profile = store
            .get_document_profile(file_id)
            .expect("read profile")
            .expect("profile exists");
        assert_eq!(profile.revision_id, new_revision);
        assert_eq!(profile.document_type, None);
        assert_eq!(profile.type_confidence, None);
        let pending = store
            .list_profiles_needing_classification(200)
            .expect("pending list after rebuild");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].0.file_id, file_id);
        assert_eq!(pending[0].1, "我的简历.pdf");
        // 新版本向量可读回，分类器可立即用
        assert!(store.profile_vector(&file_id).expect("vector").is_some());
    }

    #[test]
    fn profile_vectors_batch_returns_only_requested_profiles() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = CatalogStore::open(directory.path().join("fanfan.db")).expect("open store");
        let root = store
            .upsert_root(&test_root_registration())
            .expect("insert root");
        let file_a = {
            let connection = store.connect().expect("connect");
            let (file_a, _) = seed_profile_file(
                &connection,
                &root.root_id,
                "述职报告.docx",
                Uuid::now_v7(),
                &[r#"["项目经历"]"#],
                &["第一份述职内容"],
                2,
                "embedding-test",
            );
            file_a
        };
        let file_b = {
            let connection = store.connect().expect("connect");
            let (file_b, _) = seed_profile_file(
                &connection,
                &root.root_id,
                "会议纪要.md",
                Uuid::now_v7(),
                &[r#"["议程"]"#],
                &["会议议程内容"],
                2,
                "embedding-test",
            );
            file_b
        };
        store
            .refresh_document_profiles("embedding-test", 200)
            .expect("build profiles");

        // 只请求 A：返回单条
        let vectors = store.profile_vectors(&[file_a]).expect("batch vectors");
        assert_eq!(vectors.len(), 1);
        assert!(vectors.contains_key(&file_a));
        // 请求 A+B：全部返回，且向量为归一化文档向量
        let vectors = store
            .profile_vectors(&[file_a, file_b])
            .expect("batch vectors");
        assert_eq!(vectors.len(), 2);
        for vector in vectors.values() {
            let magnitude = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
            assert!(
                (magnitude - 1.0).abs() < 1e-5,
                "profile vectors must be normalized"
            );
        }
        // 空输入：空 map，不报错
        assert!(store.profile_vectors(&[]).expect("empty batch").is_empty());
        // 不存在的 id：静默跳过
        let missing = store
            .profile_vectors(&[Uuid::now_v7()])
            .expect("missing batch");
        assert!(missing.is_empty());
    }

    #[allow(dead_code, clippy::too_many_arguments)]
    fn test_memory_write_input(
        kind: MemoryKind,
        subject_type: MemoryTargetType,
        subject_id: Uuid,
        object_type: MemoryTargetType,
        object_id: Uuid,
        alias: Option<&str>,
        confidence: f32,
        source_type: MemorySource,
        status: MemoryStatus,
    ) -> MemoryWriteInput {
        MemoryWriteInput {
            kind,
            subject_type,
            subject_id,
            predicate: "is_about".to_owned(),
            object_type,
            object_id,
            alias: alias.map(str::to_owned),
            confidence,
            source_type,
            source_id: Some("session-test".to_owned()),
            status,
        }
    }

    #[test]
    fn memory_entities_dedup_by_type_and_name() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = CatalogStore::open(directory.path().join("fanfan.db")).expect("open store");
        let metadata = serde_json::json!({"title": "周晨"});
        let first = store
            .upsert_memory_entity("person", "周晨", &metadata)
            .expect("insert entity");
        let second = store
            .upsert_memory_entity("person", "周晨", &metadata)
            .expect("insert same entity");
        assert_eq!(first, second, "同名实体必须去重");
        let entities = store.list_memory_entities(10).expect("list entities");
        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0].entity_id, first);
        assert_eq!(entities[0].entity_type, "person");
        assert_eq!(entities[0].canonical_name, "周晨");
        assert_eq!(
            store
                .memory_entity_by_name("person", "周晨")
                .expect("by name")
                .unwrap()
                .entity_id,
            first
        );
        assert!(store.memory_entity_by_id(first).expect("by id").is_some());
        assert!(
            store
                .memory_entity_by_id(Uuid::now_v7())
                .expect("missing")
                .is_none()
        );
    }

    #[test]
    fn memory_relation_merge_honors_source_rank_and_rejection() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = CatalogStore::open(directory.path().join("fanfan.db")).expect("open store");
        let file = Uuid::now_v7();
        let entity = Uuid::now_v7();

        // 推断类来源写入 → 即使请求 confirmed 也降为 candidate（严禁 PDF→张三）
        let inference = test_memory_write_input(
            MemoryKind::Relation,
            MemoryTargetType::Entity,
            entity,
            MemoryTargetType::File,
            file,
            None,
            0.4,
            MemorySource::DocumentInference,
            MemoryStatus::Confirmed,
        );
        let first = store.upsert_memory_relation(&inference).expect("insert");
        let stored = store
            .memory_relation_by_id(first)
            .expect("read relation")
            .expect("exists");
        assert_eq!(stored.status, MemoryStatus::Candidate);
        assert_eq!(stored.source_type, MemorySource::DocumentInference);

        // 更高等级来源覆盖：document_inference(2) → repeated_usage(3)
        let repeated = MemoryWriteInput {
            source_type: MemorySource::RepeatedUsage,
            confidence: 0.8,
            status: MemoryStatus::Confirmed,
            ..inference.clone()
        };
        let _ = store.upsert_memory_relation(&repeated).expect("upgrade");
        let stored = store
            .memory_relation_by_id(first)
            .expect("read relation")
            .expect("exists");
        assert_eq!(stored.status, MemoryStatus::Confirmed);
        assert_eq!(stored.source_type, MemorySource::RepeatedUsage);
        assert_eq!(stored.confidence, 0.8);

        // 低等级来源不能覆盖高等级事实
        let weaker = MemoryWriteInput {
            source_type: MemorySource::ModelInference,
            confidence: 0.9,
            status: MemoryStatus::Candidate,
            ..inference
        };
        let _ = store.upsert_memory_relation(&weaker).expect("weaker write");
        let stored = store
            .memory_relation_by_id(first)
            .expect("read relation")
            .expect("exists");
        assert_eq!(
            stored.status,
            MemoryStatus::Confirmed,
            "低等级不能降级高等级事实"
        );
        assert_eq!(stored.source_type, MemorySource::RepeatedUsage);
        assert_eq!(stored.confidence, 0.8, "低等级不能覆盖置信度");

        // 拒绝后：低等级来源不能复活，更高等级来源可以
        let other_file = Uuid::now_v7();
        let rejected = test_memory_write_input(
            MemoryKind::Relation,
            MemoryTargetType::Entity,
            entity,
            MemoryTargetType::File,
            other_file,
            None,
            0.6,
            MemorySource::UserExplicit,
            MemoryStatus::Confirmed,
        );
        let rejected_id = store.upsert_memory_relation(&rejected).expect("insert");
        store
            .update_memory_relation_status(rejected_id, MemoryStatus::Rejected)
            .expect("reject");
        let weak_revive = MemoryWriteInput {
            source_type: MemorySource::ModelInference,
            status: MemoryStatus::Candidate,
            ..rejected
        };
        let _ = store
            .upsert_memory_relation(&weak_revive)
            .expect("weak revive write");
        assert_eq!(
            store
                .memory_relation_by_id(rejected_id)
                .expect("read")
                .expect("exists")
                .status,
            MemoryStatus::Rejected,
            "被拒绝的关系不能被低等级来源复活"
        );
        let strong_revive = MemoryWriteInput {
            source_type: MemorySource::UserExplicit,
            status: MemoryStatus::Confirmed,
            confidence: 1.0,
            ..weak_revive
        };
        let _ = store
            .upsert_memory_relation(&strong_revive)
            .expect("strong revive write");
        assert_eq!(
            store
                .memory_relation_by_id(rejected_id)
                .expect("read")
                .expect("exists")
                .status,
            MemoryStatus::Confirmed,
            "用户明确表达可以复活被拒绝的关系"
        );
    }

    #[test]
    fn memory_relation_lists_filter_by_subject_object_and_status() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = CatalogStore::open(directory.path().join("fanfan.db")).expect("open store");
        let file = Uuid::now_v7();
        let entity_a = Uuid::now_v7();
        let entity_b = Uuid::now_v7();
        let relation = test_memory_write_input(
            MemoryKind::Relation,
            MemoryTargetType::Entity,
            entity_a,
            MemoryTargetType::File,
            file,
            None,
            0.5,
            MemorySource::UserExplicit,
            MemoryStatus::Confirmed,
        );
        let _ = store.upsert_memory_relation(&relation).expect("insert a");
        let candidate = MemoryWriteInput {
            status: MemoryStatus::Candidate,
            source_type: MemorySource::DocumentInference,
            subject_id: entity_b,
            predicate: "mentions".to_owned(),
            ..relation.clone()
        };
        let _ = store.upsert_memory_relation(&candidate).expect("insert b");

        // 按主体 + 状态过滤
        let confirmed = store
            .list_memory_relations_by_subject(
                MemoryTargetType::Entity,
                entity_a,
                Some(MemoryStatus::Confirmed),
                10,
            )
            .expect("by subject confirmed");
        assert_eq!(confirmed.len(), 1);
        assert_eq!(confirmed[0].object_id, file);
        let all = store
            .list_memory_relations_by_subject(MemoryTargetType::Entity, entity_a, None, 10)
            .expect("by subject all");
        assert_eq!(all.len(), 1, "entity_a 只有一条");
        let by_object = store
            .list_memory_relations_by_object(MemoryTargetType::File, file, None, 10)
            .expect("by object");
        assert_eq!(by_object.len(), 2, "两个实体都指向该文件");
        // 候选列表（writer 确定性校验用）
        let candidates = store
            .list_memory_relation_candidates(10)
            .expect("candidates");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].subject_id, entity_b);
    }

    #[test]
    fn memory_alias_upsert_bump_find_and_retarget() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = CatalogStore::open(directory.path().join("fanfan.db")).expect("open store");
        let file_a = Uuid::now_v7();
        let file_b = Uuid::now_v7();
        let input = test_memory_write_input(
            MemoryKind::Alias,
            MemoryTargetType::File,
            file_a,
            MemoryTargetType::File,
            Uuid::now_v7(),
            Some("我的简历"),
            1.0,
            MemorySource::UserExplicit,
            MemoryStatus::Confirmed,
        );
        let alias_id = store.upsert_memory_alias(&input).expect("insert alias");

        // 大小写/空白差异仍能命中（规范化匹配）
        let hits = store
            .find_memory_aliases("  我的  简历 ")
            .expect("find alias");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].alias_id, alias_id);
        assert_eq!(hits[0].target_id, file_a);
        assert_eq!(hits[0].alias, "我的简历");
        // 空别名不产生命中
        assert!(store.find_memory_aliases("   ").expect("blank").is_empty());

        // 命中计数 + 使用时间
        store.bump_memory_alias(alias_id).expect("bump");
        let hits = store.find_memory_aliases("我的简历").expect("find again");
        assert_eq!(hits[0].hit_count, 1);
        assert!(hits[0].last_used_at.is_some());

        // 同一别名重新指向新目标 → 新行；原目标保留
        let retarget = MemoryWriteInput {
            subject_id: file_b,
            ..input
        };
        let _ = store.upsert_memory_alias(&retarget).expect("retarget");
        let hits = store.find_memory_aliases("我的简历").expect("find both");
        assert_eq!(hits.len(), 2, "同一别名指向两个目标都应保留，由解析层裁决");

        // 空别名拒绝写入
        let invalid = MemoryWriteInput {
            alias: Some("   ".to_owned()),
            ..retarget
        };
        let error = store
            .upsert_memory_alias(&invalid)
            .expect_err("blank alias rejected");
        assert_eq!(error.code, "MEMORY_ALIAS_INVALID");

        // 删除
        assert!(store.delete_memory_alias(alias_id).expect("delete"));
        assert_eq!(store.list_memory_aliases(10).expect("list").len(), 1);
    }

    #[test]
    fn memory_entity_delete_cascades_relations_and_aliases() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = CatalogStore::open(directory.path().join("fanfan.db")).expect("open store");
        let entity_id = store
            .upsert_memory_entity("project", "法律项目", &serde_json::json!({}))
            .expect("insert entity");
        let file = Uuid::now_v7();
        let relation = test_memory_write_input(
            MemoryKind::Relation,
            MemoryTargetType::Entity,
            entity_id,
            MemoryTargetType::File,
            file,
            None,
            0.9,
            MemorySource::UserExplicit,
            MemoryStatus::Confirmed,
        );
        let relation_id = store
            .upsert_memory_relation(&relation)
            .expect("insert relation");
        let alias = test_memory_write_input(
            MemoryKind::Alias,
            MemoryTargetType::Entity,
            entity_id,
            MemoryTargetType::Entity,
            entity_id,
            Some("法律项目"),
            0.9,
            MemorySource::UserExplicit,
            MemoryStatus::Confirmed,
        );
        let alias_id = store.upsert_memory_alias(&alias).expect("insert alias");

        assert!(
            store
                .delete_memory_entity(entity_id)
                .expect("delete entity")
        );
        assert!(
            store
                .memory_entity_by_id(entity_id)
                .expect("entity gone")
                .is_none()
        );
        assert!(
            store
                .memory_relation_by_id(relation_id)
                .expect("relation gone")
                .is_none()
        );
        assert!(
            store
                .memory_alias_by_id(alias_id)
                .expect("alias gone")
                .is_none()
        );
        // 其他文件的关系不受影响
        assert!(
            store
                .list_memory_relations_by_object(MemoryTargetType::File, file, None, 10)
                .expect("by object")
                .is_empty()
        );
    }

    #[test]
    fn invalidate_memory_for_file_stales_relations_and_drops_aliases() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = CatalogStore::open(directory.path().join("fanfan.db")).expect("open store");
        let file = Uuid::now_v7();
        let entity = Uuid::now_v7();
        let relation = test_memory_write_input(
            MemoryKind::Relation,
            MemoryTargetType::File,
            file,
            MemoryTargetType::Entity,
            entity,
            None,
            0.9,
            MemorySource::UserExplicit,
            MemoryStatus::Confirmed,
        );
        let relation_id = store
            .upsert_memory_relation(&relation)
            .expect("insert relation");
        let alias = test_memory_write_input(
            MemoryKind::Alias,
            MemoryTargetType::File,
            file,
            MemoryTargetType::File,
            file,
            Some("我的简历"),
            0.9,
            MemorySource::UserExplicit,
            MemoryStatus::Confirmed,
        );
        store.upsert_memory_alias(&alias).expect("insert alias");

        let affected = store.invalidate_memory_for_file(file).expect("invalidate");
        assert_eq!(affected, 2);
        let stale = store
            .memory_relation_by_id(relation_id)
            .expect("read")
            .expect("exists");
        assert_eq!(
            stale.status,
            MemoryStatus::Stale,
            "文件失效后关系标记 stale，不删除"
        );
        assert!(
            store
                .find_memory_aliases("我的简历")
                .expect("aliases")
                .is_empty(),
            "指向失效文件的别名删除"
        );
    }

    #[test]
    fn memory_target_validation_rejects_missing_offline_and_unauthorized_files() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = CatalogStore::open(directory.path().join("fanfan.db")).expect("open store");
        let root = store
            .upsert_root(&test_root_registration())
            .expect("insert root");
        let (file_id, _) = {
            let connection = store.connect().expect("connect");
            seed_profile_file(
                &connection,
                &root.root_id,
                "我的简历.pdf",
                Uuid::now_v7(),
                &[r#"["项目经历"]"#],
                &["简历内容"],
                1,
                "embedding-test",
            )
        };

        // 在场 + 授权根内 → 合法
        assert!(store.memory_file_target_valid(file_id).expect("valid file"));
        // 不存在的文件 → 不合法
        assert!(
            !store
                .memory_file_target_valid(Uuid::now_v7())
                .expect("missing file")
        );
        // 文件删除（availability 离开 present）→ 不合法：别名不得把失效文件注入 scope
        store
            .connect()
            .expect("connect")
            .execute(
                "UPDATE files SET availability = 'gone' WHERE file_id = ?1",
                params![file_id.to_string()],
            )
            .expect("remove file");
        assert!(
            !store
                .memory_file_target_valid(file_id)
                .expect("offline file")
        );
        // 收藏集：不存在的 → 不合法
        assert!(
            !store
                .memory_collection_target_valid(Uuid::now_v7())
                .expect("missing collection")
        );
    }

    #[test]
    fn memory_target_rejects_file_outside_authorized_roots() {
        // 边界 (2)：别名指向「在场但不在任何已启用授权根」的文件 → 不采用，
        // 绝不把越权文件注入 scope。
        let directory = tempfile::tempdir().expect("tempdir");
        let store = CatalogStore::open(directory.path().join("fanfan.db")).expect("open store");
        let now = Utc::now().to_rfc3339();
        let rogue_file = Uuid::now_v7();
        // 注册一个根再禁用：文件在场但所属根未启用 → 不授权
        let mut rogue_registration = test_root_registration();
        rogue_registration.canonical_path = "C:\\Users\\Test\\Other".to_owned();
        rogue_registration.path_key = "c:\\users\\test\\other".to_owned();
        let rogue_root = store
            .upsert_root(&rogue_registration)
            .expect("insert rogue root");
        store
            .disable_root(&rogue_root.root_id)
            .expect("disable root");
        {
            let connection = store.connect().expect("connect");
            connection
                .execute(
                    "INSERT INTO files (file_id, canonical_path, path_key, name, extension, size_bytes, modified_at, discovered_at, availability, volume_id, display_name, mime_type, parse_status, first_seen_at, last_seen_at) VALUES (?1, ?2, ?2, '未授权.pdf', 'pdf', 1024, ?3, ?3, 'present', 'vol-rogue', '未授权.pdf', 'application/pdf', 'parsed', ?3, ?3)",
                    params![rogue_file.to_string(), "C:\\Users\\Test\\Other\\未授权.pdf".to_owned(), now],
                )
                .expect("insert rogue file");
            // membership 指向已禁用的根：不在任何授权根内
            connection
                .execute(
                    "INSERT INTO file_root_memberships (file_id, root_id, relative_path, is_primary) VALUES (?1, ?2, '未授权.pdf', 1)",
                    params![rogue_file.to_string(), rogue_root.root_id.to_string()],
                )
                .expect("insert membership to disabled root");
        }
        assert!(
            !store
                .memory_file_target_valid(rogue_file)
                .expect("rogue file"),
            "在场但在未授权根的 memory 目标一律无效"
        );
    }

    #[test]
    fn file_document_node_count_tracks_current_revision_only() {
        // 边界 (5) 的数据层：SUMMARY 截断的 nodes_total 必须以当前修订为准，
        // 旧修订节点不计入；文件不存在 → 0。
        let directory = tempfile::tempdir().expect("tempdir");
        let store = CatalogStore::open(directory.path().join("fanfan.db")).expect("open store");
        let root = store
            .upsert_root(&test_root_registration())
            .expect("insert root");
        let revision_id = Uuid::now_v7();
        let file_id = {
            let connection = store.connect().expect("connect");
            let (file_id, _) = seed_profile_file(
                &connection,
                &root.root_id,
                "长文档.pdf",
                revision_id,
                &[r#"["一"]"#, r#"["二"]"#, r#"["三"]"#],
                &["一", "二", "三"],
                0,
                "embedding-test",
            );
            file_id
        };
        {
            // 旧修订的孤立节点（文件行从不指向它）不得被计数
            let connection = store.connect().expect("connect");
            let stale_revision = Uuid::now_v7();
            connection
                .execute(
                    "INSERT INTO file_revisions (revision_id, file_id, size_bytes, fs_modified_at, metadata_fingerprint, created_at, parse_status) VALUES (?1, ?2, 1024, ?3, ?4, ?3, 'parsed')",
                    params![stale_revision.to_string(), file_id.to_string(), Utc::now().to_rfc3339(), "stale".to_owned()],
                )
                .expect("insert stale revision");
            let stale_node = Uuid::now_v7();
            connection
                .execute(
                    "INSERT INTO document_nodes (node_id, revision_id, ordinal, node_type, locator_json, heading_path_json, text) VALUES (?1, ?2, 0, 'text', '{}', '[]', '旧版本')",
                    params![stale_node.to_string(), stale_revision.to_string()],
                )
                .expect("insert stale-revision node");
        }
        assert_eq!(
            store.file_document_node_count(&file_id).expect("count"),
            3,
            "只统计当前修订的 document_nodes"
        );
        assert_eq!(
            store
                .file_document_node_count(&Uuid::now_v7())
                .expect("missing file"),
            0,
            "文件不存在 → 0（不报错）"
        );
    }

    #[test]
    fn future_database_version_is_rejected_without_downgrade() {
        let directory = tempfile::tempdir().expect("tempdir");
        let database_path = directory.path().join("fanfan.db");
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
        let database_path = directory.path().join("fanfan.db");
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
        let database_path = directory.path().join("fanfan.db");
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
        let store = CatalogStore::open(directory.path().join("fanfan.db")).expect("open store");
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
    fn node_traces_are_recorded_and_paginated_with_filters() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = CatalogStore::open(directory.path().join("fanfan.db")).expect("open store");

        for index in 0..3 {
            store
                .record_node_trace(
                    "ask",
                    if index == 0 { "routing" } else { "retrieval" },
                    &format!("corr-{index}"),
                    Some("session-1"),
                    None,
                    &serde_json::json!({ "question": format!("问题{index}") }),
                    &serde_json::json!({ "result": index }),
                    "ok",
                    Some(index as u64 * 10),
                )
                .expect("record trace");
        }
        store
            .record_node_trace(
                "search",
                "retrieval",
                "corr-search",
                None,
                None,
                &serde_json::json!({ "query": "找资料" }),
                &serde_json::json!({ "count": 5 }),
                "error",
                None,
            )
            .expect("record search trace");

        // 无过滤分页：4 条
        let first = store
            .query_node_traces(&NodeTraceQuery {
                flow: None,
                node: None,
                cursor: None,
                page_size: 2,
            })
            .expect("first page");
        assert_eq!(first.total, 4);
        assert_eq!(first.items.len(), 2);
        assert!(first.next_cursor.is_some());
        let second = store
            .query_node_traces(&NodeTraceQuery {
                flow: None,
                node: None,
                cursor: first.next_cursor,
                page_size: 2,
            })
            .expect("second page");
        assert_eq!(second.items.len(), 2);
        assert!(second.next_cursor.is_none());

        // flow 过滤：ask 只有 3 条
        let ask_page = store
            .query_node_traces(&NodeTraceQuery {
                flow: Some("ask".into()),
                node: None,
                cursor: None,
                page_size: 10,
            })
            .expect("ask page");
        assert_eq!(ask_page.total, 3);
        assert!(ask_page.items.iter().all(|item| item.flow == "ask"));

        // flow+node 过滤：retrieval 2 条（跨 ask/search），node 过滤只看 ask.retrieval
        let node_page = store
            .query_node_traces(&NodeTraceQuery {
                flow: Some("ask".into()),
                node: Some("retrieval".into()),
                cursor: None,
                page_size: 10,
            })
            .expect("node page");
        assert_eq!(node_page.total, 2);
        assert!(
            node_page
                .items
                .iter()
                .all(|item| item.flow == "ask" && item.node == "retrieval")
        );
        assert_eq!(
            node_page.items[0].input_json["question"],
            serde_json::json!("问题2")
        );

        // 明文存储：不被 sanitize 脱敏
        assert_eq!(
            node_page.items[0].input_json["question"],
            serde_json::json!("问题2")
        );
        assert_eq!(node_page.items[0].elapsed_ms, Some(20));
    }

    #[test]
    fn node_traces_are_queryable_by_correlation_in_flow_order() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = CatalogStore::open(directory.path().join("fanfan.db")).expect("open store");

        for (index, node) in ["source_routing", "query_parsing", "retrieval", "completed"]
            .iter()
            .enumerate()
        {
            store
                .record_node_trace(
                    "ask",
                    node,
                    "corr-ask-1",
                    Some("session-1"),
                    None,
                    &serde_json::json!({ "question": "我的简历里有没有 LangGraph" }),
                    &serde_json::json!({ "step": index }),
                    "ok",
                    Some(index as u64 * 10),
                )
                .expect("record ask trace");
        }
        // 另一 correlation 与另一 flow 不应混入
        store
            .record_node_trace(
                "ask",
                "source_routing",
                "corr-ask-2",
                None,
                None,
                &serde_json::json!({ "question": "别的" }),
                &serde_json::json!({ "step": 99 }),
                "ok",
                None,
            )
            .expect("record other correlation");
        store
            .record_node_trace(
                "search",
                "retrieval",
                "corr-ask-1",
                None,
                None,
                &serde_json::json!({ "query": "搜索" }),
                &serde_json::json!({ "count": 1 }),
                "ok",
                None,
            )
            .expect("record other flow");

        let records = store
            .query_node_traces_by_correlation("ask", "corr-ask-1")
            .expect("query by correlation");
        assert_eq!(records.len(), 4);
        // 时间正序 = 流水顺序
        assert_eq!(records[0].node, "source_routing");
        assert_eq!(records[1].node, "query_parsing");
        assert_eq!(records[2].node, "retrieval");
        assert_eq!(records[3].node, "completed");
        assert_eq!(records[2].elapsed_ms, Some(20));
        assert!(records.iter().all(|record| record.flow == "ask"));

        let empty = store
            .query_node_traces_by_correlation("ask", "corr-missing")
            .expect("empty query");
        assert!(empty.is_empty());
    }

    #[test]
    fn node_traces_are_trimmed_beyond_cap_and_cleared() {
        let directory = tempfile::tempdir().expect("tempdir");
        let database_path = directory.path().join("fanfan.db");
        let store = CatalogStore::open(&database_path).expect("open store");

        // 批量直插 20000 条（单事务），逼近裁剪上限——逐条走 API 会因逐条 fsync 慢到超时
        {
            let mut connection = Connection::open(&database_path).expect("open raw connection");
            let now = Utc::now().to_rfc3339();
            let transaction = connection.transaction().expect("begin bulk insert");
            for index in 0..20_000_u32 {
                transaction
                    .execute(
                        "INSERT INTO node_traces (trace_id, flow, node, correlation_id, session_id, entity_id, input_json, output_json, status, elapsed_ms, created_at) VALUES (?1, 'ask', 'retrieval', ?2, NULL, NULL, ?3, ?4, 'ok', NULL, ?5)",
                        params![
                            Uuid::now_v7().to_string(),
                            format!("corr-{index}"),
                            serde_json::json!({ "question": index }).to_string(),
                            serde_json::json!({ "ok": true }).to_string(),
                            now,
                        ],
                    )
                    .expect("bulk insert");
            }
            transaction.commit().expect("commit bulk insert");
        }

        // 第 20001 条走正式 API → 触发裁剪到 20000
        store
            .record_node_trace(
                "ask",
                "retrieval",
                "corr-last",
                None,
                None,
                &serde_json::json!({ "question": 20000 }),
                &serde_json::json!({ "ok": true }),
                "ok",
                None,
            )
            .expect("record over cap");

        let page = store
            .query_node_traces(&NodeTraceQuery {
                flow: None,
                node: None,
                cursor: None,
                page_size: 1,
            })
            .expect("count traces");
        assert_eq!(page.total, 20000);
        assert_eq!(page.items.len(), 1);

        let cleared = store.clear_node_traces().expect("clear traces");
        assert_eq!(cleared, 20000);
        let empty = store
            .query_node_traces(&NodeTraceQuery {
                flow: None,
                node: None,
                cursor: None,
                page_size: 10,
            })
            .expect("empty page");
        assert_eq!(empty.total, 0);
        assert!(empty.items.is_empty());
    }

    #[test]
    fn structured_logs_redact_document_text_and_absolute_paths() {
        let directory = tempfile::tempdir().expect("tempdir");
        let database_path = directory.path().join("fanfan.db");
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
        let store = CatalogStore::open(directory.path().join("fanfan.db")).expect("open store");
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
        let store = CatalogStore::open(directory.path().join("fanfan.db")).expect("open store");
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
        let store = CatalogStore::open(directory.path().join("fanfan.db")).expect("open store");
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

        let first_file_page = store
            .query_files(&FileQuery {
                cursor: None,
                page_size: 1,
                query: None,
                extensions: vec![],
                parse_statuses: vec![],
                availability: None,
            })
            .expect("query first file page");
        assert_eq!(first_file_page.items.len(), 1);
        assert!(first_file_page.has_more);
        let second_file_page = store
            .query_files(&FileQuery {
                cursor: first_file_page.next_cursor.clone(),
                page_size: 1,
                query: None,
                extensions: vec![],
                parse_statuses: vec![],
                availability: None,
            })
            .expect("query second file page");
        assert_eq!(second_file_page.items.len(), 1);
        assert!(!second_file_page.has_more);
        assert_ne!(
            first_file_page.items[0].file_id,
            second_file_page.items[0].file_id
        );

        let first_inbox_page = store
            .query_inbox(&InboxQuery {
                status: TriageStatus::New,
                event_types: vec![InboxEventType::Discovered],
                root_ids: vec![root.root_id],
                date_from: None,
                date_to: None,
                cursor: None,
                page_size: 1,
            })
            .expect("query first inbox page");
        assert_eq!(first_inbox_page.items.len(), 1);
        assert!(first_inbox_page.has_more);
        let second_inbox_page = store
            .query_inbox(&InboxQuery {
                status: TriageStatus::New,
                event_types: vec![InboxEventType::Discovered],
                root_ids: vec![root.root_id],
                date_from: None,
                date_to: None,
                cursor: first_inbox_page.next_cursor.clone(),
                page_size: 1,
            })
            .expect("query second inbox page");
        assert_eq!(second_inbox_page.items.len(), 1);
        assert!(!second_inbox_page.has_more);
        assert_ne!(
            first_inbox_page.items[0].inbox_id,
            second_inbox_page.items[0].inbox_id
        );

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
        let scoped = store
            .search(&crate::SearchRequest {
                query: inbox.items[0].display_name.clone(),
                scope: ScopeFilter {
                    root_ids: vec![],
                    collection_ids: vec![manual.collection_id],
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
            .expect("search collection scope");
        assert_eq!(scoped.results.len(), 1);
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
                relation_type: None,
                review_status: None,
            })
            .expect("first relation page");
        assert_eq!(first_relation_page.total, relations.len() as u64);
        assert_eq!(first_relation_page.items.len(), 1);
        assert_eq!(
            store
                .query_file_relations(&RelationQuery {
                    cursor: first_relation_page.next_cursor,
                    page_size: 1,
                    relation_type: None,
                    review_status: None,
                })
                .expect("second relation page")
                .items
                .len(),
            relations.len().saturating_sub(1).min(1)
        );
        let exact_relations = store
            .query_file_relations(&RelationQuery {
                cursor: None,
                page_size: 20,
                relation_type: Some(RelationType::ExactDuplicate),
                review_status: Some("suggested".into()),
            })
            .expect("filter exact pending relations");
        assert_eq!(exact_relations.items.len(), 1);
        let reviewed = store
            .review_file_relations(&[exact_relations.items[0].relation_id], "accepted")
            .expect("batch review relation");
        assert_eq!(reviewed, 1);
        assert_eq!(
            store
                .query_file_relations(&RelationQuery {
                    cursor: None,
                    page_size: 20,
                    relation_type: Some(RelationType::ExactDuplicate),
                    review_status: Some("accepted".into()),
                })
                .expect("filter accepted relations")
                .items
                .len(),
            1
        );
        assert_eq!(
            fs::read_to_string(first_path).expect("read source after analysis"),
            "离线资料内容一致"
        );
    }

    #[test]
    fn parsed_chinese_content_is_searchable_with_real_locator() {
        let directory = tempfile::tempdir().expect("tempdir");
        let database_path = directory.path().join("fanfan.db");
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
                ocr_confidence: None,
                ocr_engine: None,
                vision_route_reason: None,
                description: None,
                vision_model_id: None,
                status: "pending_ocr".into(),
                error: None,
            }],
            ocr_attempts: vec![],
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
            mode: SearchMode::Hybrid,
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
        let first_ocr_claim = store
            .claim_pending_image_ocr("ocr-test")
            .expect("claim image OCR")
            .expect("pending image OCR");
        assert_eq!(first_ocr_claim.asset_id, asset_id);
        assert_eq!(first_ocr_claim.attempt_count, 1);
        assert_eq!(
            store
                .recover_interrupted_image_ocr()
                .expect("recover image OCR"),
            1
        );
        let recovered_ocr_claim = store
            .claim_pending_image_ocr("ocr-test")
            .expect("claim recovered image OCR")
            .expect("recovered pending image OCR");
        store
            .commit_image_ocr(&ImageOcrResult {
                asset_id,
                revision_id,
                model_artifact_id: "ocr-test".into(),
                ocr_text: Some("第二季度 收入 128 万元".into()),
                confidence: Some(0.42),
                engine: "rapidocr".into(),
                model_version: Some("test".into()),
                vision_required: true,
                route_reason: "complex_chart_like".into(),
                attempts: vec![OcrAttempt {
                    engine: "rapidocr".into(),
                    model_version: Some("test".into()),
                    status: "completed".into(),
                    page_no: None,
                    confidence: Some(0.42),
                    fallback_reason: Some("complex_chart_like".into()),
                    elapsed_ms: 3,
                    error: None,
                }],
                idempotency_key: recovered_ocr_claim.idempotency_key,
            })
            .expect("route complex image from OCR to vision");
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
        assert_eq!(
            image_preview.image_assets[0].ocr_engine.as_deref(),
            Some("rapidocr")
        );
        assert_eq!(image_preview.image_assets[0].ocr_confidence, Some(0.42));
        let ask = AskRequest {
            question: "第二季度收入".into(),
            session_id: None,
            scope: request.scope.clone(),
            answer_style: crate::AnswerStyle::Concise,
            retrieval_limit: 12,
            max_source_files: 8,
            strict_evidence: true,
            clarification_selection: None,
            think_mode: false,
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
        let image_search = store
            .search(&crate::SearchRequest {
                query: "第二季度收入".into(),
                scope: request.scope.clone(),
                mode: SearchMode::Fulltext,
                sort: crate::SearchSort::Relevance,
                page_size: 10,
                cursor: None,
            })
            .expect("search image understanding text");
        assert_eq!(image_search.results[0].image_asset_id, Some(asset_id));
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
    fn extractive_answers_carry_neighbor_chunk_context() {
        let directory = tempfile::tempdir().expect("tempdir");
        let database_path = directory.path().join("fanfan.db");
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
                params![file_id.to_string(), "C:\\Users\\Test\\Documents\\邻居上下文.md", "c:\\users\\test\\documents\\邻居上下文.md", "邻居上下文.md", now.to_rfc3339(), revision_id.to_string()],
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
                "INSERT INTO file_root_memberships (file_id, root_id, relative_path, is_primary) VALUES (?1, ?2, '邻居上下文.md', 1)",
                params![file_id.to_string(), root.root_id.to_string()],
            )
            .expect("insert membership");
        drop(connection);
        store
            .mark_file_parsing(&file_id, &revision_id)
            .expect("mark parsing");
        store.recover_interrupted_parses().expect("recover parse");
        store
            .mark_file_parsing(&file_id, &revision_id)
            .expect("restart parsing");

        // 单节点超长文本：切块后形成 甲（上文）→ 乙（正文）→ 丙（下文）三块。
        // 每段 480 token（边界符在目标位之外触发），块间 64 token 重叠。
        let segment = |head: &str| {
            let unit = format!("{head}。");
            unit.repeat(96) // 96 × 5 字 = 480 token
        };
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
                text: Some(format!(
                    "{}{}{}",
                    segment("上文甲段"),
                    segment("正文乙段"),
                    segment("下文丙段")
                )),
                table_data: None,
                locator: SourceLocator {
                    kind: crate::SourceKind::Text,
                    line_start: Some(1),
                    line_end: Some(1),
                    ..SourceLocator::default()
                },
                heading_path: vec!["测试".into()],
            }],
            image_assets: vec![],
            ocr_attempts: vec![],
            warnings: vec![],
            metrics: crate::ParseMetrics {
                page_count: 0,
                node_count: 1,
                character_count: 1440,
                ocr_page_count: 0,
                elapsed_ms: 1,
            },
            error: None,
        };
        store
            .commit_parse_result(&file_id, &parse_result)
            .expect("commit parse result");

        let answer = store
            .answer_extractively(
                &AskRequest {
                    question: "正文乙段".into(),
                    session_id: None,
                    scope: crate::ScopeFilter {
                        root_ids: vec![],
                        collection_ids: vec![],
                        file_ids: vec![],
                        extensions: vec![],
                        modified_from: None,
                        modified_to: None,
                        availability: crate::Availability::Present,
                    },
                    answer_style: crate::AnswerStyle::Concise,
                    retrieval_limit: 12,
                    max_source_files: 8,
                    strict_evidence: true,
                    clarification_selection: None,
                    think_mode: false,
                },
                None,
            )
            .expect("answer extractively");
        assert!(!answer.claims.is_empty(), "正文乙段应命中乙块");
        let evidence = answer.claims[0].citations[0].clone();
        assert!(evidence.quote.contains("正文乙段"), "命中块正文应包含乙段");
        // 精确契约：邻居上下文 = 同节点相邻块文本按 NEIGHBOR_CONTEXT_TOKEN_CAP
        // 截断，与库里真实相邻块逐字节一致。
        let connection = store.connect().expect("connect");
        let chunks = connection
            .prepare("SELECT ordinal, text FROM chunks ORDER BY ordinal")
            .expect("prepare chunks")
            .query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })
            .expect("query chunks")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect chunks");
        assert!(chunks.len() >= 3, "应切出至少甲乙丙三段对应块");
        // 命中块 = 含乙段、不含丙段的那块（丙块同时含乙段尾巴与丙段）
        let hit = chunks
            .iter()
            .position(|(_, text)| text.contains("正文乙段") && !text.contains("下文丙段"))
            .expect("命中中间块");
        assert_eq!(
            evidence.context_before.as_deref(),
            Some(crate::indexing::cap_by_estimated_tokens(&chunks[hit - 1].1, 128).as_str()),
            "前邻居应为甲块文本按上限截断"
        );
        assert_eq!(
            evidence.context_after.as_deref(),
            Some(crate::indexing::cap_by_estimated_tokens(&chunks[hit + 1].1, 128).as_str()),
            "后邻居应为丙块文本按上限截断"
        );
        // 语义抽查：前邻居纯甲段、后邻居含丙段（在完整块文本上成立）
        assert!(chunks[hit - 1].1.contains("上文甲段"));
        assert!(!chunks[hit - 1].1.contains("正文乙段"));
        assert!(chunks[hit + 1].1.contains("下文丙段"));
    }

    #[test]
    fn search_cursor_pages_results_and_rejects_a_changed_index() {
        let directory = tempfile::tempdir().expect("tempdir");
        let store = CatalogStore::open(directory.path().join("fanfan.db")).expect("open store");
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
        let store = CatalogStore::open(directory.path().join("fanfan.db")).expect("open store");
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
            mode: SearchMode::Hybrid,
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
        let limit_ms = std::env::var("FANFAN_SEMANTIC_P95_MS")
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

    #[test]
    fn relation_groups_cluster_refresh_query_and_review_flow() {
        let directory = tempfile::tempdir().expect("tempdir");
        let first_path = directory.path().join("归航计划-最终版.txt");
        let second_path = directory.path().join("归航计划-v2.txt");
        let third_path = directory.path().join("周报-3月.txt");
        fs::write(&first_path, "项目归航计划内容").expect("write first fixture");
        fs::write(&second_path, "项目归航计划内容").expect("write second fixture");
        fs::write(&third_path, "三月份工作周报内容").expect("write third fixture");
        let store = CatalogStore::open(directory.path().join("fanfan.db")).expect("open store");
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
            .prepare_scan_job(&root.root_id, "relations_test")
            .expect("prepare scan");
        store
            .mark_scan_running(&root.root_id, &job.job_id)
            .expect("start scan");
        let now = Utc::now();
        let discovered = |path: &std::path::Path, name: &str, offset_secs: i64| DiscoveredFile {
            volume_id: "vol-test".into(),
            windows_file_id: None,
            canonical_path: path.to_string_lossy().to_string(),
            path_key: path.to_string_lossy().to_lowercase(),
            name: name.into(),
            extension: "txt".into(),
            mime_type: "text/plain".into(),
            size_bytes: fs::metadata(path).expect("metadata").len(),
            created_at: Some(now + chrono::Duration::seconds(offset_secs)),
            modified_at: now + chrono::Duration::seconds(offset_secs),
            relative_path: name.into(),
        };
        store
            .commit_scan(
                &root.root_id,
                &job.job_id,
                &ScanOutcome {
                    files: vec![
                        discovered(&first_path, "归航计划-最终版.txt", 3),
                        discovered(&second_path, "归航计划-v2.txt", 2),
                        discovered(&third_path, "周报-3月.txt", 1),
                    ],
                    ..ScanOutcome::default()
                },
            )
            .expect("commit scan");

        // 内容哈希相同的两份 → exact_duplicate + version_candidate 边
        let refresh = store
            .refresh_file_relations(100)
            .expect("refresh relations");
        assert_eq!(refresh.exact_duplicate_pairs, 1);
        assert_eq!(refresh.version_candidate_pairs, 1);

        // 聚类落库：两个文件应聚成 1 个版本族组
        let groups_created = store.refresh_relation_groups(None).expect("refresh groups");
        assert_eq!(groups_created, 1);
        let page = store
            .query_relation_groups(&RelationGroupQuery {
                cursor: None,
                page_size: 50,
                group_type: None,
                review_status: None,
            })
            .expect("query groups");
        assert_eq!(page.total, 1);
        assert_eq!(page.items.len(), 1);
        let group = &page.items[0];
        assert_eq!(group.group_type, RelationGroupType::VersionFamily);
        assert_eq!(group.member_count, 2);
        assert_eq!(group.members.len(), 2);
        assert_eq!(group.review_status, "suggested");
        // 修改时间最新的是「归航计划-最终版」（offset 3）
        let latest = group
            .members
            .iter()
            .find(|member| member.role == RelationGroupRole::Latest)
            .expect("latest member");
        assert_eq!(latest.file.display_name, "归航计划-最终版.txt");

        // 组级复核：组和组内边一起变 accepted
        store
            .review_relation_group(&group.group_id, "accepted")
            .expect("review group");
        let after = store
            .query_relation_groups(&RelationGroupQuery {
                cursor: None,
                page_size: 50,
                group_type: None,
                review_status: Some("accepted".into()),
            })
            .expect("query accepted groups");
        assert_eq!(after.total, 1);
        let edges = store
            .query_file_relations(&RelationQuery {
                cursor: None,
                page_size: 50,
                relation_type: None,
                review_status: Some("accepted".into()),
            })
            .expect("query accepted edges");
        assert_eq!(edges.total, 2);

        // 重新聚类：suggested 组被替换，accepted 组保留
        let groups_created = store
            .refresh_relation_groups(None)
            .expect("refresh groups again");
        assert_eq!(groups_created, 1);
        let suggested = store
            .query_relation_groups(&RelationGroupQuery {
                cursor: None,
                page_size: 50,
                group_type: None,
                review_status: Some("suggested".into()),
            })
            .expect("query suggested groups");
        assert_eq!(suggested.total, 1);
        let accepted = store
            .query_relation_groups(&RelationGroupQuery {
                cursor: None,
                page_size: 50,
                group_type: None,
                review_status: Some("accepted".into()),
            })
            .expect("query accepted groups");
        assert_eq!(accepted.total, 1);

        // 排除后不再聚类
        store
            .review_relation_group(&suggested.items[0].group_id, "rejected")
            .expect("reject group");
        let groups_created = store
            .refresh_relation_groups(None)
            .expect("refresh groups after reject");
        assert_eq!(groups_created, 0);
    }
}
