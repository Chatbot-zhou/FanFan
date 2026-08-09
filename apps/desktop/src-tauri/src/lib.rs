mod commands;

use std::{
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, atomic::AtomicBool},
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
    CatalogService, IncrementalWatchManager, LocalGenerationRuntime, ModelManager, ThemeService,
    WelcomeService, WorkerClient,
};
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
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.unminimize();
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        .on_window_event(|window, event| {
            if matches!(event, tauri::WindowEvent::CloseRequested { .. }) {
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
            }
        })
        .setup(|app| {
            let config_dir = app.path().app_config_dir()?;
            let local_data_dir = app.path().app_local_data_dir()?;
            apply_pending_data_reset(&config_dir, &local_data_dir)?;
            let data_dir = app.path().app_data_dir()?;
            app.manage(WelcomeServiceState(Mutex::new(WelcomeService::new(
                config_dir.clone(),
                "1.0",
            ))));
            app.manage(ThemeServiceState(Mutex::new(ThemeService::new(config_dir))));
            app.manage(EnvironmentServiceState {
                data_directory: data_dir.clone(),
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
            });
            let packaged_llama = app
                .path()
                .resource_dir()?
                .join("runtime")
                .join("llama")
                .join("llama-server.exe");
            let llama_executable = if packaged_llama.is_file() {
                packaged_llama
            } else {
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("../../../.artifacts/runtime/llama/llama-server.exe")
            };
            app.manage(GenerationServiceState(Arc::new(Mutex::new(
                LocalGenerationRuntime::new(llama_executable),
            ))));
            app.manage(AskCoordinatorState::default());
            app.manage(ModelDownloadCoordinatorState::default());

            let startup_app = app.handle().clone();
            tauri::async_runtime::spawn_blocking(move || {
                initialize_background_services(startup_app, data_dir, catalog, models, startup);
            });
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
            app_data::model_download_start,
            app_data::model_download_list,
            app_data::model_download_get,
            app_data::model_download_pause,
            app_data::model_download_cancel,
            app_data::model_download_retry,
            app_data::model_artifact_activate,
            app_data::home_get_summary,
            app_data::candidate_root_action,
            app_data::search_start,
            app_data::ask_start,
            app_data::ask_operation_get,
            app_data::ask_cancel,
            app_data::preview_get,
            app_data::file_open,
            app_data::file_reveal,
            app_data::inbox_query,
            app_data::inbox_update,
            app_data::ocr_retry,
            app_data::image_understanding_retry,
            app_data::image_deep_analyze,
            app_data::knowledge_space_list,
            app_data::knowledge_space_create,
            app_data::knowledge_space_update,
            app_data::knowledge_space_delete,
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
            app_data::file_query,
            app_data::exclusion_rule_list,
            app_data::exclusion_rule_upsert,
            app_data::exclusion_rule_delete,
            app_data::extraction_preset_list,
            app_data::extraction_run,
            app_data::skill_list,
            app_data::task_plan,
            app_data::task_execute,
            app_data::task_recoverable,
            app_data::task_resume,
            app_data::extraction_export,
            app_data::maintenance_get,
            app_data::maintenance_check,
            app_data::storage_usage_get,
            app_data::storage_policy_set,
            app_data::cache_clear,
            app_data::app_data_reset_schedule,
            app_data::maintenance_log_query,
            app_data::maintenance_logs_clear,
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

fn apply_pending_data_reset(config_dir: &Path, local_data_dir: &Path) -> std::io::Result<()> {
    let parent = config_dir.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "application config directory has no parent",
        )
    })?;
    let marker = parent.join(".com.remin.desktop-reset-request");
    if !marker.is_file() {
        return Ok(());
    }
    if fs::read_to_string(&marker)?.trim() != "RESET_APPLICATION_DATA" {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "application reset marker is invalid",
        ));
    }
    let timestamp = Utc::now().format("%Y%m%d-%H%M%S");
    let mut targets = vec![("roaming", config_dir)];
    if local_data_dir != config_dir {
        targets.push(("local", local_data_dir));
    }
    for (kind, target) in targets {
        if !target.exists() {
            continue;
        }
        if !target.is_absolute()
            || target.file_name().and_then(|name| name.to_str()) != Some("com.remin.desktop")
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
        let quarantine = target_parent.join(format!("com.remin.desktop.reset-{timestamp}-{kind}"));
        if quarantine.exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "application reset quarantine already exists",
            ));
        }
        fs::rename(target, quarantine)?;
    }
    fs::remove_file(marker)
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
        catalog.recover_interrupted_tasks()?;
        let recovered = catalog.recover_interrupted_scans()?;
        let roots = catalog.list_roots()?;
        let event_app = app.clone();
        let event_catalog = Arc::clone(&catalog);
        let handler = Arc::new(
            move |result: Result<remin_core::JobRecord, remin_core::AppError>| match result {
                Ok(job) => {
                    let should_parse = matches!(
                        job.status,
                        remin_core::JobStatus::Succeeded | remin_core::JobStatus::Partial
                    );
                    let _ = event_app.emit("job.progress", job);
                    if should_parse {
                        app_data::spawn_parse_pending(
                            event_app.clone(),
                            Arc::clone(&event_catalog),
                        );
                    }
                }
                Err(error) => {
                    if error.code != "SCAN_QUEUE_BUSY" {
                        let _ = event_app.emit("catalog.watch_degraded", error);
                    }
                }
            },
        );
        let mut watcher = IncrementalWatchManager::new(Arc::clone(&catalog), handler);
        for root in roots {
            if let Err(error) = watcher.watch_root(&root) {
                let _ = app.emit("catalog.watch_degraded", error);
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
        Ok(pending_files) => startup.publish(
            &app,
            StartupState {
                phase: "ready",
                ready: true,
                progress: 1.0,
                pending_files,
                blocker: None,
                recovery_actions: Vec::new(),
            },
        ),
        Err(error) => startup.fail(&app, error),
    }
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
}
