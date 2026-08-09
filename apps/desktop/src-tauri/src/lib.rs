mod commands;

use std::{
    path::PathBuf,
    sync::{Arc, Mutex, atomic::AtomicBool},
};

use commands::{
    app_data::{
        self, AskCoordinatorState, CatalogServiceState, EnvironmentServiceState,
        GenerationServiceState, ModelServiceState, WatcherServiceState, WorkerServiceState,
    },
    startup::{StartupServiceState, StartupState},
    welcome::WelcomeServiceState,
};
use remin_core::{
    CatalogService, IncrementalWatchManager, LocalGenerationRuntime, ModelManager, WelcomeService,
    WorkerClient,
};
use tauri::{Emitter, Manager};

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.unminimize();
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let config_dir = app.path().app_config_dir()?;
            let data_dir = app.path().app_data_dir()?;
            app.manage(WelcomeServiceState(Mutex::new(WelcomeService::new(
                config_dir, "1.0",
            ))));
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
            app_data::environment_get_latest,
            app_data::environment_detect,
            app_data::model_state_get,
            app_data::model_import_scan,
            app_data::model_import_confirm,
            app_data::model_artifact_list,
            app_data::model_catalog_list,
            app_data::model_download_install,
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
            app_data::collection_list,
            app_data::collection_create,
            app_data::collection_update,
            app_data::collection_delete,
            app_data::collection_rule_preview,
            app_data::collection_file_query,
            app_data::collection_add_file,
            app_data::collection_remove_file,
            app_data::relation_refresh,
            app_data::relation_query,
            app_data::relation_review,
            app_data::file_query,
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
            app_data::maintenance_log_query,
            app_data::maintenance_logs_clear,
            app_data::diagnostic_export,
            app_data::index_rebuild,
            app_data::root_discover_defaults,
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
            degradation_level: "full",
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
                degradation_level: "full",
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
                degradation_level: "full",
                blocker: None,
                recovery_actions: Vec::new(),
            },
        );
        catalog.recover_interrupted_parses()?;
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
                degradation_level: "full",
                blocker: None,
                recovery_actions: Vec::new(),
            },
        );
        app_data::spawn_scan_queue(app.clone(), Arc::clone(&catalog), recovered);
        app_data::spawn_parse_pending(app.clone(), Arc::clone(&catalog));
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
                degradation_level: "full",
                blocker: None,
                recovery_actions: Vec::new(),
            },
        ),
        Err(error) => startup.fail(&app, error),
    }
}
