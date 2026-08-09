use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, RecvTimeoutError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use notify::{
    Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher,
    event::{ModifyKind, RenameMode},
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{AppError, CatalogService, JobRecord, RootRecord, WatchMode, path_key};

const DEBOUNCE_WINDOW: Duration = Duration::from_secs(2);
const STABILITY_SAMPLE_INTERVAL: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FileSystemEventType {
    Created,
    Modified,
    Removed,
    Renamed,
}

impl FileSystemEventType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Modified => "modified",
            Self::Removed => "removed",
            Self::Renamed => "renamed",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileSystemEvent {
    pub event_type: FileSystemEventType,
    pub observed_path: String,
    pub previous_path: Option<String>,
    pub observed_at: DateTime<Utc>,
}

#[derive(Debug, Default)]
pub struct EventCoalescer {
    pending: HashMap<String, FileSystemEvent>,
    last_event_at: Option<Instant>,
}

impl EventCoalescer {
    pub fn push_at(&mut self, event: FileSystemEvent, now: Instant) {
        let key = path_key(Path::new(&event.observed_path));
        self.pending
            .entry(key)
            .and_modify(|existing| *existing = merge_events(existing, &event))
            .or_insert(event);
        self.last_event_at = Some(now);
    }

    pub fn take_ready_at(&mut self, now: Instant) -> Vec<FileSystemEvent> {
        if self
            .last_event_at
            .is_none_or(|last_event_at| now.duration_since(last_event_at) < DEBOUNCE_WINDOW)
        {
            return Vec::new();
        }
        self.last_event_at = None;
        self.pending.drain().map(|(_, event)| event).collect()
    }
}

fn merge_events(existing: &FileSystemEvent, incoming: &FileSystemEvent) -> FileSystemEvent {
    let event_type = match (existing.event_type, incoming.event_type) {
        (_, FileSystemEventType::Removed) => FileSystemEventType::Removed,
        (_, FileSystemEventType::Renamed) => FileSystemEventType::Renamed,
        (FileSystemEventType::Created, FileSystemEventType::Modified) => {
            FileSystemEventType::Created
        }
        _ => incoming.event_type,
    };
    FileSystemEvent {
        event_type,
        observed_path: incoming.observed_path.clone(),
        previous_path: incoming
            .previous_path
            .clone()
            .or_else(|| existing.previous_path.clone()),
        observed_at: incoming.observed_at,
    }
}

fn translate_notify_event(event: Event) -> Vec<FileSystemEvent> {
    let observed_at = Utc::now();
    if matches!(
        event.kind,
        EventKind::Modify(ModifyKind::Name(RenameMode::Both))
    ) && event.paths.len() >= 2
    {
        return vec![FileSystemEvent {
            event_type: FileSystemEventType::Renamed,
            observed_path: event.paths[1].to_string_lossy().into_owned(),
            previous_path: Some(event.paths[0].to_string_lossy().into_owned()),
            observed_at,
        }];
    }
    let event_type = match event.kind {
        EventKind::Create(_) => FileSystemEventType::Created,
        EventKind::Modify(_) => FileSystemEventType::Modified,
        EventKind::Remove(_) => FileSystemEventType::Removed,
        _ => return Vec::new(),
    };
    event
        .paths
        .into_iter()
        .map(|path| FileSystemEvent {
            event_type,
            observed_path: path.to_string_lossy().into_owned(),
            previous_path: None,
            observed_at,
        })
        .collect()
}

fn files_are_stable(events: &[FileSystemEvent]) -> bool {
    let candidates = events
        .iter()
        .filter(|event| event.event_type != FileSystemEventType::Removed)
        .map(|event| PathBuf::from(&event.observed_path))
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();
    let first = candidates
        .iter()
        .map(|path| std::fs::metadata(path).map(|metadata| metadata.len()))
        .collect::<Result<Vec<_>, _>>();
    let Ok(first) = first else {
        return false;
    };
    if candidates.is_empty() {
        return true;
    }
    thread::sleep(STABILITY_SAMPLE_INTERVAL);
    candidates
        .iter()
        .map(|path| std::fs::metadata(path).map(|metadata| metadata.len()))
        .collect::<Result<Vec<_>, _>>()
        .is_ok_and(|second| first == second)
}

type WatchResultHandler = Arc<dyn Fn(Result<JobRecord, AppError>) + Send + Sync + 'static>;

struct WatchedRoot {
    _watcher: RecommendedWatcher,
    shutdown: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl WatchedRoot {
    fn start(
        root_id: Uuid,
        path: &Path,
        catalog: Arc<CatalogService>,
        handler: WatchResultHandler,
    ) -> Result<Self, AppError> {
        let (sender, receiver) = mpsc::channel();
        let mut watcher = notify::recommended_watcher(sender)
            .map_err(|error| AppError::new("WATCHER_START_FAILED", error.to_string(), true))?;
        watcher
            .watch(path, RecursiveMode::Recursive)
            .map_err(|error| AppError::new("WATCHER_ROOT_FAILED", error.to_string(), true))?;
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_shutdown = Arc::clone(&shutdown);
        let worker = thread::spawn(move || {
            let mut coalescer = EventCoalescer::default();
            while !worker_shutdown.load(Ordering::Relaxed) {
                match receiver.recv_timeout(Duration::from_millis(100)) {
                    Ok(Ok(event)) => {
                        for translated in translate_notify_event(event) {
                            coalescer.push_at(translated, Instant::now());
                        }
                    }
                    Ok(Err(error)) => handler(Err(AppError::new(
                        "WATCHER_EVENT_FAILED",
                        error.to_string(),
                        true,
                    ))),
                    Err(RecvTimeoutError::Disconnected) => break,
                    Err(RecvTimeoutError::Timeout) => {}
                }
                let ready = coalescer.take_ready_at(Instant::now());
                if ready.is_empty() {
                    continue;
                }
                if !files_are_stable(&ready) {
                    for event in ready {
                        coalescer.push_at(event, Instant::now());
                    }
                    continue;
                }
                let result = catalog.apply_incremental_events(&root_id, &ready);
                if result.as_ref().is_err_and(|error| error.retryable) {
                    for event in ready {
                        coalescer.push_at(event, Instant::now());
                    }
                }
                handler(result);
            }
        });
        Ok(Self {
            _watcher: watcher,
            shutdown,
            worker: Some(worker),
        })
    }
}

impl Drop for WatchedRoot {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub struct IncrementalWatchManager {
    catalog: Arc<CatalogService>,
    handler: WatchResultHandler,
    roots: HashMap<Uuid, WatchedRoot>,
}

impl IncrementalWatchManager {
    pub fn new(catalog: Arc<CatalogService>, handler: WatchResultHandler) -> Self {
        Self {
            catalog,
            handler,
            roots: HashMap::new(),
        }
    }

    pub fn watch_root(&mut self, root: &RootRecord) -> Result<bool, AppError> {
        if !root.enabled || root.watch_mode != WatchMode::Realtime {
            return Ok(false);
        }
        if self.roots.contains_key(&root.root_id) {
            return Ok(false);
        }
        let watched = WatchedRoot::start(
            root.root_id,
            Path::new(&root.canonical_path),
            Arc::clone(&self.catalog),
            Arc::clone(&self.handler),
        )?;
        self.roots.insert(root.root_id, watched);
        Ok(true)
    }

    pub fn watched_root_count(&self) -> usize {
        self.roots.len()
    }

    pub fn unwatch_root(&mut self, root_id: &Uuid) -> bool {
        self.roots.remove(root_id).is_some()
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::{JobStatus, RootSource};

    fn event(event_type: FileSystemEventType, path: &str) -> FileSystemEvent {
        FileSystemEvent {
            event_type,
            observed_path: path.to_owned(),
            previous_path: None,
            observed_at: Utc::now(),
        }
    }

    #[test]
    fn events_wait_for_two_second_quiet_window() {
        let start = Instant::now();
        let mut coalescer = EventCoalescer::default();
        coalescer.push_at(
            event(FileSystemEventType::Modified, "C:\\资料\\计划.docx"),
            start,
        );

        assert!(
            coalescer
                .take_ready_at(start + Duration::from_millis(1999))
                .is_empty()
        );
        assert_eq!(
            coalescer
                .take_ready_at(start + Duration::from_secs(2))
                .len(),
            1
        );
    }

    #[test]
    fn create_followed_by_modify_stays_created() {
        let start = Instant::now();
        let mut coalescer = EventCoalescer::default();
        coalescer.push_at(
            event(FileSystemEventType::Created, "C:\\资料\\计划.docx"),
            start,
        );
        coalescer.push_at(
            event(FileSystemEventType::Modified, "C:\\资料\\计划.docx"),
            start + Duration::from_millis(50),
        );

        let ready = coalescer.take_ready_at(start + Duration::from_millis(2050));
        assert_eq!(ready[0].event_type, FileSystemEventType::Created);
    }

    #[test]
    fn remove_overrides_prior_modify() {
        let start = Instant::now();
        let mut coalescer = EventCoalescer::default();
        coalescer.push_at(
            event(FileSystemEventType::Modified, "C:\\资料\\计划.docx"),
            start,
        );
        coalescer.push_at(
            event(FileSystemEventType::Removed, "C:\\资料\\计划.docx"),
            start + Duration::from_millis(50),
        );

        let ready = coalescer.take_ready_at(start + Duration::from_millis(2050));
        assert_eq!(ready[0].event_type, FileSystemEventType::Removed);
    }

    #[cfg(windows)]
    #[test]
    fn native_watcher_persists_and_applies_a_debounced_event() {
        let app_data = tempfile::tempdir().expect("app data tempdir");
        let source = tempfile::tempdir().expect("source tempdir");
        let catalog = Arc::new(
            CatalogService::open(app_data.path().to_path_buf()).expect("open catalog service"),
        );
        let root = catalog
            .register_folder(
                "测试资料",
                source.path().to_path_buf(),
                RootSource::UserFolder,
            )
            .expect("register root");
        let initial = catalog
            .prepare_scan(&root.root_id, "initial")
            .expect("prepare initial scan");
        catalog
            .execute_scan(root.root_id, initial.job.job_id)
            .expect("execute initial scan");

        let (sender, receiver) = mpsc::channel();
        let handler = Arc::new(move |result| {
            let _ = sender.send(result);
        });
        let mut manager = IncrementalWatchManager::new(Arc::clone(&catalog), handler);
        assert!(manager.watch_root(&root).expect("watch root"));
        assert_eq!(manager.watched_root_count(), 1);

        let document = source.path().join("增量资料.txt");
        fs::write(&document, "增量监听不会修改源文件").expect("write watched file");
        let before = fs::read(&document).expect("read source before processing");
        let job = receiver
            .recv_timeout(Duration::from_secs(15))
            .expect("watch result")
            .expect("watch processing succeeds");

        assert_eq!(job.status, JobStatus::Succeeded);
        assert_eq!(catalog.list_files().expect("list files").len(), 1);
        assert_eq!(
            fs::read(document).expect("read source after processing"),
            before
        );
    }
}
