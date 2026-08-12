mod commands;
mod runtime_log;

use std::{
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, atomic::AtomicBool},
    time::Instant,
};

use chrono::Utc;
use commands::{
    app_data::{
        self, AskCoordinatorState, CatalogServiceState, EnvironmentServiceState,
        GenerationServiceState, ModelDownloadCoordinatorState, ModelServiceState,
        ScanCoordinatorState, WatcherServiceState, WorkerServiceState,
    },
    startup::{StartupServiceState, StartupState},
    theme::ThemeServiceState,
    welcome::WelcomeServiceState,
};
use remin_core::{
    CatalogService, IncrementalWatchManager, LocalGenerationRuntime, ModelManager,
    RuntimeCapability, ThemeService, WelcomeService, WorkerClient,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::{Emitter, Manager};

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .register_uri_scheme_protocol("remin-pdf", |context, request| {
            pdf_protocol_response(context.app_handle(), context.webview_label(), &request)
        })
        .register_uri_scheme_protocol("remin-image", |context, request| {
            image_protocol_response(context.app_handle(), context.webview_label(), &request)
        })
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            runtime_log::event(
                "info",
                "application",
                "single_instance.focus_requested",
                None,
                &serde_json::json!({}),
            );
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.unminimize();
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        .on_window_event(|window, event| {
            if matches!(event, tauri::WindowEvent::CloseRequested { .. }) {
                runtime_log::event(
                    "info",
                    "application",
                    "window.close_requested",
                    None,
                    &serde_json::json!({ "window": "main" }),
                );
                window
                    .app_handle()
                    .state::<ModelDownloadCoordinatorState>()
                    .pause_all();
                let worker = window.app_handle().state::<WorkerServiceState>();
                worker.client.cancel_active();
                if let Ok(mut generation) = window
                    .app_handle()
                    .state::<GenerationServiceState>()
                    .0
                    .lock()
                {
                    generation.stop();
                }
                runtime_log::mark_clean_shutdown();
            }
        })
        .setup(|app| {
            let config_dir = app.path().app_config_dir()?;
            let local_data_dir = app.path().app_local_data_dir()?;
            apply_pending_data_reset(&config_dir, &local_data_dir)?;
            let default_data_dir = app.path().app_data_dir()?;
            let data_dir = resolve_application_data_directory(&config_dir, &default_data_dir);
            let (log_initialization, log_fallback_error) =
                match runtime_log::initialize(data_dir.join("logs")) {
                    Ok(initialization) => (initialization, None),
                    Err(primary_error) if data_dir != local_data_dir => (
                        runtime_log::initialize(local_data_dir.join("logs"))?,
                        Some(primary_error),
                    ),
                    Err(error) => return Err(error.into()),
                };
            runtime_log::install_panic_hook();
            if let Some(error) = log_fallback_error {
                runtime_log::event(
                    "warning",
                    "application",
                    "runtime_log.fallback_activated",
                    None,
                    &serde_json::json!({
                        "error_code": error.code,
                        "retryable": error.retryable,
                    }),
                );
            }
            runtime_log::event(
                "info",
                "application",
                "setup.started",
                None,
                &serde_json::json!({
                    "session_id": log_initialization.session_id,
                    "previous_session_unclean": log_initialization.previous_session_unclean,
                    "debug_build": cfg!(debug_assertions),
                }),
            );
            app.manage(WelcomeServiceState(Mutex::new(WelcomeService::new(
                config_dir.clone(),
                "1.0",
            ))));
            app.manage(ThemeServiceState(Mutex::new(ThemeService::new(
                config_dir.clone(),
            ))));
            app.manage(EnvironmentServiceState {
                data_directory: data_dir.clone(),
                config_directory: config_dir.clone(),
                latest: Mutex::new(None),
            });
            let startup = StartupServiceState::default();
            app.manage(startup.clone());
            let models = ModelServiceState::default();
            let catalog = CatalogServiceState::default();
            app.manage(models.clone());
            app.manage(catalog.clone());
            app.manage(WatcherServiceState::default());
            app.manage(ScanCoordinatorState::default());
            let packaged_worker = app
                .path()
                .resource_dir()?
                .join("worker")
                .join("remin-worker.exe");
            let worker_client = if packaged_worker.is_file() {
                WorkerClient::from_executable(packaged_worker)
            } else {
                let worker_root =
                    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../services/worker");
                WorkerClient::from_environment(worker_root)
            };
            app.manage(WorkerServiceState {
                client: worker_client,
                running: AtomicBool::new(false),
                embedding_running: AtomicBool::new(false),
                embedding_reschedule: AtomicBool::new(false),
                vision_running: AtomicBool::new(false),
                foreground_activity: std::sync::atomic::AtomicU32::new(0),
            });
            let packaged_runtime_root = app.path().resource_dir()?.join("runtime");
            let development_runtime_root =
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../.artifacts/runtime");
            let managed_runtime_root = data_dir.join("runtime");
            let gpu_candidates = [
                managed_runtime_root.join("llama-cuda/llama-server.exe"),
                managed_runtime_root.join("llama-vulkan/llama-server.exe"),
                packaged_runtime_root.join("llama-cuda/llama-server.exe"),
                packaged_runtime_root.join("llama-vulkan/llama-server.exe"),
                development_runtime_root.join("llama-cuda/llama-server.exe"),
                development_runtime_root.join("llama-vulkan/llama-server.exe"),
            ];
            let cpu_candidates = [
                managed_runtime_root.join("llama/llama-server.exe"),
                packaged_runtime_root.join("llama/llama-server.exe"),
                development_runtime_root.join("llama/llama-server.exe"),
            ];
            let cpu_executable = cpu_candidates
                .into_iter()
                .find(|path| path.is_file())
                .unwrap_or_else(|| development_runtime_root.join("llama/llama-server.exe"));
            // Start with the CPU runtime immediately so setup never blocks on
            // device probing; the GPU probe runs in the background and swaps
            // the runtime in once it completes (cold GPU bring-up can take
            // tens of seconds, which used to freeze the whole window).
            let generation_inner = Arc::new(Mutex::new(LocalGenerationRuntime::new(
                cpu_executable.clone(),
            )));
            app.manage(GenerationServiceState(Arc::clone(&generation_inner)));
            runtime_log::event(
                "info",
                "model.runtime",
                "probe.started",
                None,
                &serde_json::json!({ "candidate_count": gpu_candidates.len() }),
            );
            let probe_generation = Arc::clone(&generation_inner);
            tauri::async_runtime::spawn_blocking(move || {
                background_probe_generation_runtime(
                    gpu_candidates.to_vec(),
                    cpu_executable,
                    probe_generation,
                );
            });
            app.manage(AskCoordinatorState::default());
            app.manage(ModelDownloadCoordinatorState::default());

            let startup_app = app.handle().clone();
            tauri::async_runtime::spawn_blocking(move || {
                initialize_background_services(startup_app, data_dir, catalog, models, startup);
            });
            runtime_log::event(
                "info",
                "application",
                "setup.completed",
                None,
                &serde_json::json!({ "background_initialization_started": true }),
            );
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::startup::startup_get_state,
            commands::welcome::welcome_get_state,
            commands::welcome::welcome_complete,
            commands::welcome::welcome_authorization_complete,
            commands::theme::theme_get_state,
            commands::theme::theme_set_preference,
            app_data::environment_get_latest,
            app_data::environment_detect,
            app_data::model_state_get,
            app_data::rag_readiness_get,
            app_data::model_import_scan,
            app_data::model_import_confirm,
            app_data::model_artifact_list,
            app_data::model_role_config_list,
            app_data::model_catalog_list,
            app_data::model_role_catalog_list,
            app_data::model_preset_list,
            app_data::model_download_start,
            app_data::model_download_list,
            app_data::model_download_get,
            app_data::model_download_pause,
            app_data::model_download_cancel,
            app_data::model_download_retry,
            app_data::model_artifact_activate,
            app_data::model_role_disable,
            app_data::home_get_summary,
            app_data::candidate_root_action,
            app_data::search_start,
            app_data::ask_start,
            app_data::ask_session_query,
            app_data::ask_message_query,
            app_data::ask_session_rename,
            app_data::ask_session_delete,
            app_data::ask_operation_get,
            app_data::ask_cancel,
            app_data::preview_get,
            app_data::file_open,
            app_data::file_reveal,
            app_data::inbox_query,
            app_data::inbox_update,
            app_data::inbox_retry,
            app_data::ocr_retry,
            app_data::image_understanding_retry,
            app_data::image_deep_analyze,
            app_data::collection_list,
            app_data::collection_create,
            app_data::collection_update,
            app_data::collection_delete,
            app_data::collection_rule_preview,
            app_data::collection_file_query,
            app_data::collection_add_file,
            app_data::collection_remove_file,
            app_data::collection_suggestion_refresh,
            app_data::collection_suggestion_query,
            app_data::collection_suggestion_update,
            app_data::collection_suggestion_confirm,
            app_data::collection_suggestion_reject,
            app_data::relation_refresh,
            app_data::relation_query,
            app_data::relation_review,
            app_data::relation_batch_review,
            app_data::file_query,
            app_data::answer_export,
            app_data::exclusion_rule_list,
            app_data::exclusion_rule_upsert,
            app_data::exclusion_rule_delete,
            app_data::app_status_get,
            app_data::maintenance_get,
            app_data::maintenance_check,
            app_data::storage_usage_get,
            app_data::storage_location_get,
            app_data::storage_migration_schedule,
            app_data::cache_clear,
            app_data::app_data_reset_schedule,
            app_data::maintenance_log_query,
            app_data::maintenance_logs_clear,
            app_data::diagnostic_event_append,
            app_data::diagnostic_export,
            app_data::index_rebuild,
            app_data::root_list,
            app_data::root_add,
            app_data::root_disable,
            app_data::scan_start,
            app_data::scan_pause,
            app_data::scan_resume,
            app_data::scan_cancel,
        ])
        .run(tauri::generate_context!())
        .expect("拾忆桌面应用启动失败");
}

const STORAGE_LOCATION_FILE: &str = "storage-location.json";
const MANAGED_STORAGE_MARKER: &str = ".remin-managed-data-v1";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct StorageLocationConfig {
    active_data_directory: Option<String>,
    pending: Option<PendingStorageMigration>,
    last_error: Option<String>,
    updated_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PendingStorageMigration {
    source_directory: String,
    target_directory: String,
    requested_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct StorageLocationStatus {
    active_data_directory: String,
    pending_target_directory: Option<String>,
    restart_required: bool,
    last_error: Option<String>,
}

pub(crate) fn storage_location_status(
    config_dir: &Path,
    current_data_dir: &Path,
) -> StorageLocationStatus {
    let config = read_storage_location_config(config_dir).unwrap_or_default();
    StorageLocationStatus {
        active_data_directory: current_data_dir.to_string_lossy().into_owned(),
        pending_target_directory: config
            .pending
            .as_ref()
            .map(|pending| pending.target_directory.clone()),
        restart_required: config.pending.is_some(),
        last_error: config.last_error,
    }
}

pub(crate) fn schedule_storage_migration(
    config_dir: &Path,
    current_data_dir: &Path,
    selected_parent: &Path,
    authorized_roots: &[PathBuf],
) -> Result<StorageLocationStatus, remin_core::AppError> {
    if !selected_parent.is_absolute() || !selected_parent.is_dir() {
        return Err(remin_core::AppError::new(
            "STORAGE_MIGRATION_TARGET_INVALID",
            "请选择一个已存在的本地文件夹作为新存储位置",
            false,
        ));
    }
    let selected_parent = selected_parent.canonicalize().map_err(|_| {
        remin_core::AppError::new(
            "STORAGE_MIGRATION_TARGET_INVALID",
            "无法读取所选存储位置",
            true,
        )
    })?;
    let selected_text = selected_parent.to_string_lossy();
    let is_unc = selected_text.starts_with("\\\\")
        && (!selected_text.starts_with("\\\\?\\")
            || selected_text
                .to_ascii_lowercase()
                .starts_with("\\\\?\\unc\\"));
    if is_unc {
        return Err(remin_core::AppError::new(
            "STORAGE_MIGRATION_TARGET_INVALID",
            "应用数据只能迁移到本地磁盘，不能使用网络位置",
            false,
        ));
    }
    let target = selected_parent.join("ReminData");
    if path_contains(current_data_dir, &target) || path_contains(&target, current_data_dir) {
        return Err(remin_core::AppError::new(
            "STORAGE_MIGRATION_TARGET_INVALID",
            "新存储位置不能位于当前应用数据目录内部或包含当前目录",
            false,
        ));
    }
    if authorized_roots
        .iter()
        .any(|root| path_contains(root, &target))
    {
        return Err(remin_core::AppError::new(
            "STORAGE_MIGRATION_TARGET_AUTHORIZED_SOURCE",
            "索引和模型目录不能放在已授权的资料源目录中",
            false,
        ));
    }
    if target.exists()
        && target
            .read_dir()
            .map_err(|_| {
                remin_core::AppError::new(
                    "STORAGE_MIGRATION_TARGET_INVALID",
                    "无法检查新存储目录",
                    true,
                )
            })?
            .next()
            .is_some()
    {
        return Err(remin_core::AppError::new(
            "STORAGE_MIGRATION_TARGET_NOT_EMPTY",
            "所选位置中的ReminData目录不是空目录，请选择其他位置",
            false,
        ));
    }
    let mut config = read_storage_location_config(config_dir).unwrap_or_default();
    config.active_data_directory = Some(current_data_dir.to_string_lossy().into_owned());
    config.pending = Some(PendingStorageMigration {
        source_directory: current_data_dir.to_string_lossy().into_owned(),
        target_directory: target.to_string_lossy().into_owned(),
        requested_at: Utc::now().to_rfc3339(),
    });
    config.last_error = None;
    config.updated_at = Some(Utc::now().to_rfc3339());
    write_storage_location_config(config_dir, &config).map_err(|_| {
        remin_core::AppError::new(
            "STORAGE_MIGRATION_SCHEDULE_FAILED",
            "无法保存存储迁移计划",
            true,
        )
    })?;
    Ok(storage_location_status(config_dir, current_data_dir))
}

fn resolve_application_data_directory(config_dir: &Path, default_data_dir: &Path) -> PathBuf {
    let mut config = read_storage_location_config(config_dir).unwrap_or_default();
    let active = config
        .active_data_directory
        .as_deref()
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| default_data_dir.to_path_buf());
    let Some(pending) = config.pending.clone() else {
        return active;
    };
    let source = PathBuf::from(&pending.source_directory);
    let target = PathBuf::from(&pending.target_directory);
    let result = copy_tree_verified(&source, &target);
    match result {
        Ok(()) => {
            config.active_data_directory = Some(target.to_string_lossy().into_owned());
            config.pending = None;
            config.last_error = None;
            config.updated_at = Some(Utc::now().to_rfc3339());
            if write_storage_location_config(config_dir, &config).is_ok() {
                target
            } else {
                source
            }
        }
        Err(_) => {
            config.last_error = Some(
                "存储迁移未完成，拾忆已继续使用原位置；请检查目标磁盘空间和写入权限后重试".into(),
            );
            config.updated_at = Some(Utc::now().to_rfc3339());
            let _ = write_storage_location_config(config_dir, &config);
            source
        }
    }
}

fn read_storage_location_config(config_dir: &Path) -> std::io::Result<StorageLocationConfig> {
    let path = config_dir.join(STORAGE_LOCATION_FILE);
    if !path.is_file() {
        return Ok(StorageLocationConfig::default());
    }
    serde_json::from_slice(&fs::read(path)?)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

fn write_storage_location_config(
    config_dir: &Path,
    config: &StorageLocationConfig,
) -> std::io::Result<()> {
    fs::create_dir_all(config_dir)?;
    let target = config_dir.join(STORAGE_LOCATION_FILE);
    let temporary = config_dir.join(format!("{STORAGE_LOCATION_FILE}.tmp"));
    let backup = config_dir.join(format!("{STORAGE_LOCATION_FILE}.bak"));
    let payload = serde_json::to_vec_pretty(config)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    let mut file = File::create(&temporary)?;
    file.write_all(&payload)?;
    file.sync_all()?;
    if backup.exists() {
        fs::remove_file(&backup)?;
    }
    if target.exists() {
        fs::rename(&target, &backup)?;
    }
    if let Err(error) = fs::rename(&temporary, &target) {
        if backup.exists() {
            let _ = fs::rename(&backup, &target);
        }
        return Err(error);
    }
    if backup.exists() {
        fs::remove_file(backup)?;
    }
    Ok(())
}

fn copy_tree_verified(source: &Path, target: &Path) -> std::io::Result<()> {
    if !source.is_absolute() || !source.is_dir() || !target.is_absolute() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid storage migration paths",
        ));
    }
    if path_contains(source, target) || path_contains(target, source) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "overlapping storage migration paths",
        ));
    }
    copy_directory_entries(source, target)?;
    verify_directory_entries(source, target)?;
    let marker = target.join(MANAGED_STORAGE_MARKER);
    let mut marker_file = File::create(marker)?;
    marker_file.write_all(b"REMIN_MANAGED_DATA_V1")?;
    marker_file.sync_all()
}

fn copy_directory_entries(source: &Path, target: &Path) -> std::io::Result<()> {
    fs::create_dir_all(target)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "storage data contains a symbolic link",
            ));
        }
        let destination = target.join(entry.file_name());
        if metadata.is_dir() {
            copy_directory_entries(&entry.path(), &destination)?;
        } else if metadata.is_file() {
            if destination.is_file()
                && fs::metadata(&destination)?.len() == metadata.len()
                && sha256_path(&entry.path())? == sha256_path(&destination)?
            {
                continue;
            }
            let temporary = destination.with_extension("remin-copying");
            if temporary.exists() {
                fs::remove_file(&temporary)?;
            }
            fs::copy(entry.path(), &temporary)?;
            if sha256_path(&entry.path())? != sha256_path(&temporary)? {
                fs::remove_file(&temporary)?;
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "copied storage file hash mismatch",
                ));
            }
            if destination.exists() {
                fs::remove_file(&destination)?;
            }
            fs::rename(temporary, destination)?;
        }
    }
    Ok(())
}

fn verify_directory_entries(source: &Path, target: &Path) -> std::io::Result<()> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let destination = target.join(entry.file_name());
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.is_dir() {
            if !destination.is_dir() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "storage migration directory missing",
                ));
            }
            verify_directory_entries(&entry.path(), &destination)?;
        } else if metadata.is_file()
            && (!destination.is_file()
                || fs::metadata(&destination)?.len() != metadata.len()
                || sha256_path(&entry.path())? != sha256_path(&destination)?)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "storage migration verification failed",
            ));
        }
    }
    Ok(())
}

fn sha256_path(path: &Path) -> std::io::Result<String> {
    let mut file = File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn path_contains(parent: &Path, child: &Path) -> bool {
    let normalize = |path: &Path| {
        path.to_string_lossy()
            .replace('/', "\\")
            .trim_start_matches("\\\\?\\")
            .to_lowercase()
    };
    let parent = normalize(parent);
    let child = normalize(child);
    child == parent || child.starts_with(&format!("{}\\", parent.trim_end_matches('\\')))
}

fn apply_pending_data_reset(config_dir: &Path, local_data_dir: &Path) -> std::io::Result<()> {
    let parent = config_dir.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "application config directory has no parent",
        )
    })?;
    let reset_marker = parent.join(".com.remin.desktop-reset-request");
    if !reset_marker.is_file() {
        return Ok(());
    }
    if fs::read_to_string(&reset_marker)?.trim() != "RESET_APPLICATION_DATA" {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "application reset marker is invalid",
        ));
    }
    let timestamp = Utc::now().format("%Y%m%d-%H%M%S");
    let external_active = read_storage_location_config(config_dir)
        .ok()
        .and_then(|config| config.active_data_directory)
        .map(PathBuf::from)
        .filter(|path| path != config_dir && path != local_data_dir);
    let mut targets = vec![("roaming", config_dir.to_path_buf())];
    if local_data_dir != config_dir {
        targets.push(("local", local_data_dir.to_path_buf()));
    }
    if let Some(external_active) = external_active {
        targets.push(("migrated", external_active));
    }
    for (kind, target) in targets {
        if !target.exists() {
            continue;
        }
        let expected_name = if kind == "migrated" {
            "ReminData"
        } else {
            "com.remin.desktop"
        };
        let managed_marker_valid = kind != "migrated"
            || fs::read_to_string(target.join(MANAGED_STORAGE_MARKER))
                .is_ok_and(|value| value.trim() == "REMIN_MANAGED_DATA_V1");
        if !target.is_absolute()
            || target.file_name().and_then(|name| name.to_str()) != Some(expected_name)
            || !managed_marker_valid
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "refusing to reset an unexpected application data path",
            ));
        }
        let target_parent = target.parent().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "application data directory has no parent",
            )
        })?;
        let quarantine = target_parent.join(format!("{expected_name}.reset-{timestamp}-{kind}"));
        if quarantine.exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "application reset quarantine already exists",
            ));
        }
        if let Err(error) = fs::rename(&target, &quarantine) {
            // The setup hook runs after the main window's WebView2 has already
            // started, so the local data directory's EBWebView profile folder
            // is held open and renaming it fails with ERROR_ACCESS_DENIED —
            // which previously panicked on every launch and left the reset
            // marker in place, bricking the app. Quarantine every sibling
            // entry instead and leave the profile in place; it is a
            // regenerated cache, not application data.
            if kind == "local"
                && error.kind() == std::io::ErrorKind::PermissionDenied
                && target.join("EBWebView").is_dir()
            {
                quarantine_local_data_skipping_webview(&target, &quarantine)?;
            } else {
                return Err(error);
            }
        }
    }
    fs::remove_file(reset_marker)
}

/// Fallback used when the whole local data directory cannot be renamed at
/// reset time because the app's own WebView2 keeps the EBWebView profile
/// folder open: quarantine every sibling entry, then best-effort clear the
/// profile contents that are not in use (the profile regenerates itself).
fn quarantine_local_data_skipping_webview(target: &Path, quarantine: &Path) -> std::io::Result<()> {
    fs::create_dir(quarantine)?;
    for entry in fs::read_dir(target)? {
        let entry = entry?;
        if entry.file_name() == "EBWebView" {
            continue;
        }
        fs::rename(entry.path(), quarantine.join(entry.file_name()))?;
    }
    clear_webview_profile_cache(&target.join("EBWebView"));
    Ok(())
}

/// Best-effort clear of the WebView2 profile contents; in-use files are
/// skipped silently, everything else is removed.
fn clear_webview_profile_cache(directory: &Path) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            let _ = fs::remove_dir_all(entry.path());
        } else {
            let _ = fs::remove_file(entry.path());
        }
    }
}

fn pdf_protocol_response(
    app: &tauri::AppHandle,
    webview_label: &str,
    request: &tauri::http::Request<Vec<u8>>,
) -> tauri::http::Response<Vec<u8>> {
    use tauri::http::{Response, StatusCode, header};

    let error_response = |status: StatusCode, message: &str| {
        Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .body(message.as_bytes().to_vec())
            .expect("valid PDF protocol error response")
    };
    if webview_label != "main" {
        return error_response(StatusCode::FORBIDDEN, "forbidden");
    }
    if request.method() == tauri::http::Method::OPTIONS {
        return Response::builder()
            .status(StatusCode::NO_CONTENT)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .header(header::ACCESS_CONTROL_ALLOW_HEADERS, "Range")
            .header(header::ACCESS_CONTROL_ALLOW_METHODS, "GET, HEAD, OPTIONS")
            .body(Vec::new())
            .expect("valid PDF protocol preflight response");
    }
    if request.method() != tauri::http::Method::GET && request.method() != tauri::http::Method::HEAD
    {
        return error_response(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
    }
    let Some(file_id) = request
        .uri()
        .path()
        .trim_matches('/')
        .split('/')
        .next()
        .and_then(|value| uuid::Uuid::parse_str(value).ok())
    else {
        return error_response(StatusCode::BAD_REQUEST, "invalid file id");
    };
    let Some(catalog_state) = app.try_state::<CatalogServiceState>() else {
        return error_response(StatusCode::SERVICE_UNAVAILABLE, "catalog unavailable");
    };
    let path = match catalog_state
        .get()
        .and_then(|catalog| catalog.authorized_file_path(&file_id))
    {
        Ok(path) if is_pdf(&path) => path,
        Ok(_) => return error_response(StatusCode::UNSUPPORTED_MEDIA_TYPE, "not a PDF"),
        Err(_) => return error_response(StatusCode::NOT_FOUND, "file unavailable"),
    };
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(_) => return error_response(StatusCode::NOT_FOUND, "file unavailable"),
    };
    let total = match file.metadata().map(|metadata| metadata.len()) {
        Ok(total) if total > 0 => total,
        _ => return error_response(StatusCode::NOT_FOUND, "file unavailable"),
    };
    let range = request
        .headers()
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| parse_byte_range(value, total));
    let (status, start, end) = range
        .map(|(start, end)| (StatusCode::PARTIAL_CONTENT, start, end))
        .unwrap_or((StatusCode::OK, 0, total - 1));
    let length = end - start + 1;
    let mut body = if request.method() == tauri::http::Method::HEAD {
        Vec::new()
    } else {
        let Ok(length) = usize::try_from(length) else {
            return error_response(StatusCode::PAYLOAD_TOO_LARGE, "range too large");
        };
        let mut body = vec![0_u8; length];
        if file.seek(SeekFrom::Start(start)).is_err() || file.read_exact(&mut body).is_err() {
            return error_response(StatusCode::INTERNAL_SERVER_ERROR, "read failed");
        }
        body
    };
    let mut response = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/pdf")
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, length.to_string())
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .header(header::CACHE_CONTROL, "no-store")
        .header("X-Content-Type-Options", "nosniff");
    if status == StatusCode::PARTIAL_CONTENT {
        response = response.header(
            header::CONTENT_RANGE,
            format!("bytes {start}-{end}/{total}"),
        );
    }
    response
        .body(std::mem::take(&mut body))
        .expect("valid PDF protocol response")
}

fn image_protocol_response(
    app: &tauri::AppHandle,
    webview_label: &str,
    request: &tauri::http::Request<Vec<u8>>,
) -> tauri::http::Response<Vec<u8>> {
    use tauri::http::{Response, StatusCode, header};

    let error_response = |status: StatusCode, message: &str| {
        Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .header(header::CACHE_CONTROL, "no-store")
            .body(message.as_bytes().to_vec())
            .expect("valid image protocol error response")
    };
    if webview_label != "main" {
        return error_response(StatusCode::FORBIDDEN, "forbidden");
    }
    if request.method() == tauri::http::Method::OPTIONS {
        return Response::builder()
            .status(StatusCode::NO_CONTENT)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .header(header::ACCESS_CONTROL_ALLOW_METHODS, "GET, HEAD, OPTIONS")
            .body(Vec::new())
            .expect("valid image protocol preflight response");
    }
    if request.method() != tauri::http::Method::GET && request.method() != tauri::http::Method::HEAD
    {
        return error_response(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
    }
    let Some(asset_id) = request
        .uri()
        .path()
        .trim_matches('/')
        .split('/')
        .next()
        .and_then(|value| uuid::Uuid::parse_str(value).ok())
    else {
        return error_response(StatusCode::BAD_REQUEST, "invalid image asset id");
    };
    let Some(catalog_state) = app.try_state::<CatalogServiceState>() else {
        return error_response(StatusCode::SERVICE_UNAVAILABLE, "catalog unavailable");
    };
    let (path, mime_type, size_bytes) = match catalog_state
        .get()
        .and_then(|catalog| catalog.authorized_image_asset_path(&asset_id))
    {
        Ok(asset) => asset,
        Err(_) => return error_response(StatusCode::NOT_FOUND, "image asset unavailable"),
    };
    let body = if request.method() == tauri::http::Method::HEAD {
        Vec::new()
    } else {
        match fs::read(path) {
            Ok(body) if body.len() as u64 == size_bytes => body,
            _ => return error_response(StatusCode::NOT_FOUND, "image asset unavailable"),
        }
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, mime_type)
        .header(header::CONTENT_LENGTH, size_bytes.to_string())
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .header(header::CACHE_CONTROL, "no-store")
        .header("X-Content-Type-Options", "nosniff")
        .body(body)
        .expect("valid image protocol response")
}

fn parse_byte_range(value: &str, total: u64) -> Option<(u64, u64)> {
    let value = value.strip_prefix("bytes=")?;
    if value.contains(',') {
        return None;
    }
    let (start, end) = value.split_once('-')?;
    if start.is_empty() {
        let suffix = end.parse::<u64>().ok()?.min(total);
        return (suffix > 0).then_some((total - suffix, total - 1));
    }
    let start = start.parse::<u64>().ok()?;
    if start >= total {
        return None;
    }
    let end = if end.is_empty() {
        total - 1
    } else {
        end.parse::<u64>().ok()?.min(total - 1)
    };
    (start <= end).then_some((start, end))
}

fn is_pdf(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("pdf"))
}

fn initialize_background_services(
    app: tauri::AppHandle,
    data_dir: PathBuf,
    catalog_state: CatalogServiceState,
    model_state: ModelServiceState,
    startup: StartupServiceState,
) {
    runtime_log::event(
        "info",
        "startup",
        "background_initialization.started",
        None,
        &serde_json::json!({}),
    );
    startup.publish(
        &app,
        StartupState {
            phase: "opening_catalog",
            ready: false,
            progress: 0.15,
            pending_files: 0,
            blocker: None,
            recovery_actions: Vec::new(),
        },
    );

    let initialized = (|| {
        let catalog = Arc::new(CatalogService::open(data_dir.clone())?);
        catalog_state.initialize(Arc::clone(&catalog))?;
        startup.publish(
            &app,
            StartupState {
                phase: "opening_models",
                ready: false,
                progress: 0.28,
                pending_files: 0,
                blocker: None,
                recovery_actions: Vec::new(),
            },
        );
        model_state.initialize(Arc::new(ModelManager::open(data_dir)?))?;
        startup.publish(
            &app,
            StartupState {
                phase: "recovering_jobs",
                ready: false,
                progress: 0.35,
                pending_files: 0,
                blocker: None,
                recovery_actions: Vec::new(),
            },
        );
        catalog.recover_interrupted_parses()?;
        catalog.recover_interrupted_image_understanding()?;
        let recovered = catalog.recover_interrupted_scans()?;
        let roots = catalog.list_roots()?;
        if let Err(error) = catalog.discover_candidate_roots() {
            runtime_log::event(
                "warning",
                "startup",
                "candidate_roots.discovery_failed",
                None,
                &serde_json::json!({
                    "error_code": error.code,
                    "retryable": error.retryable,
                }),
            );
        }
        runtime_log::event(
            "info",
            "startup",
            "background_initialization.recovered",
            None,
            &serde_json::json!({
                "recovered_scan_jobs": recovered.len(),
                "authorized_roots": roots.len(),
            }),
        );
        let event_app = app.clone();
        let event_catalog = Arc::clone(&catalog);
        let handler = Arc::new(
            move |result: Result<remin_core::JobRecord, remin_core::AppError>| match result {
                Ok(job) => {
                    let should_parse = matches!(
                        job.status,
                        remin_core::JobStatus::Succeeded | remin_core::JobStatus::Partial
                    );
                    let _ = event_app.emit("job:progress", &job);
                    runtime_log::event(
                        "info",
                        "watcher",
                        "incremental_scan.completed",
                        Some(&job.job_id.to_string()),
                        &serde_json::json!({
                            "job_id": job.job_id,
                            "status": &job.status,
                            "processed_items": job.processed_items,
                            "total_items": job.total_items,
                        }),
                    );
                    if should_parse {
                        app_data::spawn_parse_pending(
                            event_app.clone(),
                            Arc::clone(&event_catalog),
                        );
                    }
                }
                Err(error) => {
                    runtime_log::event(
                        if error.code == "SCAN_QUEUE_BUSY" {
                            "info"
                        } else {
                            "error"
                        },
                        "watcher",
                        "incremental_scan.failed",
                        None,
                        &serde_json::json!({
                            "error_code": &error.code,
                            "retryable": error.retryable,
                        }),
                    );
                    if error.code != "SCAN_QUEUE_BUSY" {
                        let _ = event_app.emit("catalog:watch_degraded", error);
                    }
                }
            },
        );
        let mut watcher = IncrementalWatchManager::new(Arc::clone(&catalog), handler);
        for root in roots {
            if let Err(error) = watcher.watch_root(&root) {
                runtime_log::event(
                    "warning",
                    "watcher",
                    "root_watch.failed",
                    Some(&root.root_id.to_string()),
                    &serde_json::json!({
                        "root_id": root.root_id,
                        "error_code": &error.code,
                        "retryable": error.retryable,
                    }),
                );
                let _ = app.emit("catalog:watch_degraded", error);
            } else {
                runtime_log::event(
                    "info",
                    "watcher",
                    "root_watch.started",
                    Some(&root.root_id.to_string()),
                    &serde_json::json!({
                        "root_id": root.root_id,
                        "root_kind": &root.root_kind,
                        "watch_mode": &root.watch_mode,
                    }),
                );
            }
        }
        app.state::<WatcherServiceState>().install(watcher)?;

        let pending_files = catalog.maintenance_snapshot()?.pending_files;
        startup.publish(
            &app,
            StartupState {
                phase: "scheduling_background_work",
                ready: true,
                progress: 0.9,
                pending_files,
                blocker: None,
                recovery_actions: Vec::new(),
            },
        );
        app_data::spawn_scan_queue(app.clone(), Arc::clone(&catalog), recovered);
        app_data::spawn_parse_pending(app.clone(), Arc::clone(&catalog));
        app_data::spawn_image_understanding_pending(app.clone(), Arc::clone(&catalog));
        app_data::spawn_embed_pending(app.clone(), Arc::clone(&catalog));
        Ok::<_, remin_core::AppError>(pending_files)
    })();

    match initialized {
        Ok(pending_files) => {
            runtime_log::event(
                "info",
                "startup",
                "background_initialization.completed",
                None,
                &serde_json::json!({ "pending_files": pending_files }),
            );
            startup.publish(
                &app,
                StartupState {
                    phase: "ready",
                    ready: true,
                    progress: 1.0,
                    pending_files,
                    blocker: None,
                    recovery_actions: Vec::new(),
                },
            );
        }
        Err(error) => {
            runtime_log::event(
                "error",
                "startup",
                "background_initialization.failed",
                None,
                &serde_json::json!({
                    "error_code": &error.code,
                    "retryable": error.retryable,
                }),
            );
            startup.fail(&app, error);
        }
    }
}

/// Probe the GPU runtime candidates off the main thread and swap the
/// generation runtime once a working GPU backend is found. The swap is
/// skipped if a CPU instance is already serving so in-flight work is never
/// interrupted; the new runtime only affects subsequent activations.
fn background_probe_generation_runtime(
    gpu_candidates: Vec<PathBuf>,
    cpu_executable: PathBuf,
    generation: Arc<Mutex<LocalGenerationRuntime>>,
) {
    let started = Instant::now();
    let mut best: Option<(PathBuf, RuntimeCapability)> = None;
    for path in gpu_candidates {
        if !path.is_file() {
            continue;
        }
        let capability = LocalGenerationRuntime::new(path.clone()).probe_capability();
        if capability.gpu_available {
            best = Some((path, capability));
            break;
        }
    }
    let mut fields = serde_json::json!({
        "duration_ms": started.elapsed().as_millis() as u64,
        "gpu_available": best.is_some(),
    });
    match best {
        Some((gpu, capability)) => {
            let swapped = match generation.lock() {
                Ok(mut guard) => {
                    if guard.is_active() {
                        false
                    } else {
                        *guard = LocalGenerationRuntime::new_with_fallback_and_capability(
                            gpu,
                            cpu_executable,
                            capability.clone(),
                        );
                        true
                    }
                }
                Err(_) => false,
            };
            fields["backend"] = capability.backend.clone().into();
            fields["devices"] = capability.devices.clone().into();
            fields["error_code"] = capability.error_code.clone().into();
            fields["swapped"] = swapped.into();
        }
        None => {
            fields["backend"] = "cpu".into();
            fields["swapped"] = false.into();
        }
    }
    runtime_log::event("info", "model.runtime", "probe.completed", None, &fields);
}

#[cfg(test)]
mod reset_tests {
    use super::*;

    #[test]
    fn pending_reset_moves_only_exact_application_directories_to_quarantine() {
        let base = std::env::temp_dir().join(format!("remin-reset-test-{}", uuid::Uuid::now_v7()));
        let roaming_parent = base.join("roaming");
        let local_parent = base.join("local");
        let config_dir = roaming_parent.join("com.remin.desktop");
        let local_data_dir = local_parent.join("com.remin.desktop");
        fs::create_dir_all(&config_dir).expect("create roaming data");
        fs::create_dir_all(&local_data_dir).expect("create local data");
        fs::write(config_dir.join("remin.db"), b"database").expect("write database");
        fs::write(local_data_dir.join("cache.bin"), b"cache").expect("write cache");
        fs::write(
            roaming_parent.join(".com.remin.desktop-reset-request"),
            "RESET_APPLICATION_DATA",
        )
        .expect("write reset marker");

        apply_pending_data_reset(&config_dir, &local_data_dir).expect("apply reset");

        assert!(!config_dir.exists());
        assert!(!local_data_dir.exists());
        assert!(
            fs::read_dir(&roaming_parent)
                .expect("read roaming parent")
                .any(|entry| entry
                    .expect("roaming entry")
                    .file_name()
                    .to_string_lossy()
                    .starts_with("com.remin.desktop.reset-"))
        );
        assert!(
            fs::read_dir(&local_parent)
                .expect("read local parent")
                .any(|entry| entry
                    .expect("local entry")
                    .file_name()
                    .to_string_lossy()
                    .starts_with("com.remin.desktop.reset-"))
        );
        fs::remove_dir_all(base).expect("clean exact reset test directory");
    }

    #[test]
    fn pending_storage_migration_copies_verifies_and_switches_without_removing_source() {
        let base = std::env::temp_dir().join(format!(
            "remin-storage-migration-test-{}",
            uuid::Uuid::now_v7()
        ));
        let config_dir = base.join("config");
        let source = base.join("source");
        let destination_parent = base.join("destination");
        fs::create_dir_all(source.join("indexes")).expect("create source");
        fs::create_dir_all(&destination_parent).expect("create destination parent");
        fs::write(source.join("remin.db"), b"database-v14").expect("write database");
        fs::write(
            source.join("indexes").join("active.usearch"),
            b"vector-index",
        )
        .expect("write index");

        let scheduled = schedule_storage_migration(&config_dir, &source, &destination_parent, &[])
            .expect("schedule migration");
        assert!(scheduled.restart_required);

        let expected_target = destination_parent
            .canonicalize()
            .expect("canonical destination")
            .join("ReminData");
        let active = resolve_application_data_directory(&config_dir, &source);
        assert_eq!(active, expected_target);
        assert_eq!(
            fs::read(active.join("remin.db")).expect("read migrated database"),
            b"database-v14"
        );
        assert_eq!(
            sha256_path(&source.join("indexes").join("active.usearch")).expect("source hash"),
            sha256_path(&active.join("indexes").join("active.usearch")).expect("target hash")
        );
        assert!(source.join("remin.db").is_file());
        assert!(!storage_location_status(&config_dir, &active).restart_required);
        fs::remove_dir_all(base).expect("clean storage migration test directory");
    }

    #[test]
    fn storage_migration_rejects_an_authorized_source_directory() {
        let base = std::env::temp_dir().join(format!(
            "remin-storage-safety-test-{}",
            uuid::Uuid::now_v7()
        ));
        let config_dir = base.join("config");
        let source = base.join("source");
        let authorized = base.join("authorized");
        fs::create_dir_all(&source).expect("create source");
        fs::create_dir_all(&authorized).expect("create authorized root");
        let error = schedule_storage_migration(
            &config_dir,
            &source,
            &authorized,
            std::slice::from_ref(&authorized),
        )
        .expect_err("reject index inside authorized source");
        assert_eq!(error.code, "STORAGE_MIGRATION_TARGET_AUTHORIZED_SOURCE");
        fs::remove_dir_all(base).expect("clean storage safety test directory");
    }

    #[test]
    fn reset_fallback_quarantines_local_entries_except_the_webview_profile() {
        let base = std::env::temp_dir().join(format!(
            "remin-reset-fallback-test-{}",
            uuid::Uuid::now_v7()
        ));
        let local = base.join("local").join("com.remin.desktop");
        let quarantine = base.join("local").join("com.remin.desktop.reset-fallback");
        fs::create_dir_all(local.join("EBWebView").join("Cache")).expect("create profile cache");
        fs::write(local.join("EBWebView").join("Preferences"), b"profile")
            .expect("write profile file");
        fs::create_dir_all(local.join("image-assets")).expect("create image assets");
        fs::write(local.join("image-assets").join("thumb.bin"), b"thumb").expect("write asset");

        quarantine_local_data_skipping_webview(&local, &quarantine).expect("fallback quarantine");

        assert!(quarantine.join("image-assets").join("thumb.bin").is_file());
        assert!(!local.join("image-assets").exists());
        assert!(!local.join("EBWebView").join("Cache").exists());
        assert!(!local.join("EBWebView").join("Preferences").exists());
        assert!(local.join("EBWebView").is_dir());
        fs::remove_dir_all(base).expect("clean fallback test directory");
    }

    #[test]
    fn reset_quarantines_an_activated_migrated_data_directory() {
        let base = std::env::temp_dir().join(format!(
            "remin-migrated-reset-test-{}",
            uuid::Uuid::now_v7()
        ));
        let roaming_parent = base.join("roaming");
        let config_dir = roaming_parent.join("com.remin.desktop");
        let destination_parent = base.join("other-disk");
        fs::create_dir_all(&config_dir).expect("create config data");
        fs::create_dir_all(&destination_parent).expect("create destination");
        fs::write(config_dir.join("remin.db"), b"database").expect("write database");
        schedule_storage_migration(&config_dir, &config_dir, &destination_parent, &[])
            .expect("schedule migration");
        let active = resolve_application_data_directory(&config_dir, &config_dir);
        assert!(active.join(MANAGED_STORAGE_MARKER).is_file());
        fs::write(
            roaming_parent.join(".com.remin.desktop-reset-request"),
            "RESET_APPLICATION_DATA",
        )
        .expect("write reset request");

        apply_pending_data_reset(&config_dir, &config_dir).expect("apply migrated reset");

        assert!(!config_dir.exists());
        assert!(!active.exists());
        assert!(
            fs::read_dir(&destination_parent)
                .expect("read destination parent")
                .any(|entry| entry
                    .expect("destination entry")
                    .file_name()
                    .to_string_lossy()
                    .starts_with("ReminData.reset-"))
        );
        fs::remove_dir_all(base).expect("clean migrated reset test directory");
    }
}
