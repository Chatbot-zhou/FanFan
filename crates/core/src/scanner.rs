use std::{
    collections::VecDeque,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
    thread,
    time::Duration,
    time::SystemTime,
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    AppError, BUILT_IN_EXCLUSION_RULES, EXCLUSION_RULES_VERSION, ExclusionRule, ExclusionRuleType,
};

#[cfg(windows)]
use std::os::windows::fs::MetadataExt;

#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;

#[cfg(windows)]
use windows::{
    Win32::{
        Foundation::CloseHandle,
        Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_FLAG_BACKUP_SEMANTICS,
            FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
            GetFileInformationByHandle, OPEN_EXISTING,
        },
    },
    core::PCWSTR,
};

#[derive(Debug, Clone)]
pub struct ScanPolicy {
    protected_paths: Vec<String>,
    rules: Vec<ExclusionRule>,
    pub rules_version: u32,
    pub max_files: Option<usize>,
}

impl ScanPolicy {
    pub fn new(protected_paths: impl IntoIterator<Item = PathBuf>) -> Self {
        Self {
            protected_paths: protected_paths
                .into_iter()
                .map(|path| fs::canonicalize(&path).unwrap_or(path))
                .map(|path| path_key(&path))
                .collect(),
            rules: BUILT_IN_EXCLUSION_RULES
                .iter()
                .map(|rule| ExclusionRule {
                    rule_id: uuid::Uuid::now_v7(),
                    root_id: None,
                    rule_class: rule.rule_class,
                    rule_type: rule.rule_type,
                    value: serde_json::Value::String(rule.value.to_owned()),
                    enabled: true,
                    overridable: rule.overridable,
                })
                .collect(),
            rules_version: EXCLUSION_RULES_VERSION,
            max_files: None,
        }
    }

    pub fn with_rules(mut self, rules: Vec<ExclusionRule>) -> Self {
        self.rules = rules;
        self
    }

    pub fn protects(&self, path: &Path) -> bool {
        let candidate = path_key(path);
        self.protected_paths.iter().any(|protected| {
            candidate == *protected || candidate.starts_with(&format!("{protected}\\"))
        })
    }

    pub fn excludes(&self, path: &Path, is_directory: bool) -> bool {
        let path_key_value = path_key(path);
        let name = path
            .file_name()
            .map(|value| value.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        let extension = path
            .extension()
            .map(|value| value.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        self.rules
            .iter()
            .filter(|rule| rule.enabled && rule.root_id.is_none())
            .any(|rule| {
                let value = rule.value.as_str().unwrap_or_default().to_ascii_lowercase();
                match rule.rule_type {
                    ExclusionRuleType::ExactPath => path_key_value == path_key(Path::new(&value)),
                    ExclusionRuleType::PathName => name == value,
                    ExclusionRuleType::PathGlob => wildcard_match(&value, &name),
                    ExclusionRuleType::Extension => !is_directory && extension == value,
                    ExclusionRuleType::Hidden
                    | ExclusionRuleType::System
                    | ExclusionRuleType::ReparsePoint
                    | ExclusionRuleType::CloudPlaceholder => false,
                }
            })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiscoveredFile {
    pub volume_id: String,
    pub windows_file_id: Option<String>,
    pub canonical_path: String,
    pub path_key: String,
    pub name: String,
    pub extension: String,
    pub mime_type: String,
    pub size_bytes: u64,
    pub created_at: Option<DateTime<Utc>>,
    pub modified_at: DateTime<Utc>,
    pub relative_path: String,
}

#[derive(Debug, Clone, Default)]
pub struct ScanOutcome {
    pub files: Vec<DiscoveredFile>,
    pub skipped_entries: u64,
    pub error_count: u64,
    pub deferred_by_budget: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ScanControl {
    state: Arc<AtomicU8>,
}

impl ScanControl {
    const RUNNING: u8 = 0;
    const PAUSED: u8 = 1;
    const CANCELLED: u8 = 2;

    pub fn pause(&self) {
        self.state.store(Self::PAUSED, Ordering::Release);
    }

    pub fn resume(&self) {
        self.state.store(Self::RUNNING, Ordering::Release);
    }

    pub fn cancel(&self) {
        self.state.store(Self::CANCELLED, Ordering::Release);
    }

    fn checkpoint(&self) -> Result<(), AppError> {
        loop {
            match self.state.load(Ordering::Acquire) {
                Self::RUNNING => return Ok(()),
                Self::PAUSED => thread::sleep(Duration::from_millis(50)),
                Self::CANCELLED => {
                    return Err(AppError::new(
                        "SCAN_CANCELLED",
                        "扫描任务已由用户取消",
                        false,
                    ));
                }
                _ => {
                    return Err(AppError::new(
                        "SCAN_CONTROL_INVALID",
                        "扫描控制器进入未知状态",
                        false,
                    ));
                }
            }
        }
    }
}

pub fn scan_root(root: &Path, policy: &ScanPolicy) -> Result<ScanOutcome, AppError> {
    scan_root_with_control(root, policy, &ScanControl::default())
}

pub fn scan_root_with_control(
    root: &Path,
    policy: &ScanPolicy,
    control: &ScanControl,
) -> Result<ScanOutcome, AppError> {
    if policy.protects(root) {
        return Err(AppError::new(
            "ROOT_PROTECTED",
            "拾忆的内部数据目录不能作为资料来源",
            false,
        ));
    }
    if !root.is_dir() {
        return Err(AppError::new(
            "ROOT_UNAVAILABLE",
            "资料目录不存在或当前不可读取",
            true,
        ));
    }

    let mut outcome = ScanOutcome::default();
    let canonical_root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut queue = VecDeque::from([canonical_root.clone()]);

    while let Some(directory) = queue.pop_front() {
        control.checkpoint()?;
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(_) => {
                outcome.error_count += 1;
                continue;
            }
        };

        for entry in entries {
            control.checkpoint()?;
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => {
                    outcome.error_count += 1;
                    continue;
                }
            };
            let path = entry.path();
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(_) => {
                    outcome.error_count += 1;
                    continue;
                }
            };

            if metadata.file_type().is_symlink() || is_hidden_system_or_reparse(&metadata) {
                outcome.skipped_entries += 1;
                continue;
            }

            if metadata.is_dir() {
                if policy.protects(&path) || policy.excludes(&path, true) {
                    outcome.skipped_entries += 1;
                } else {
                    queue.push_back(path);
                }
                continue;
            }

            if !metadata.is_file() || policy.excludes(&path, false) {
                outcome.skipped_entries += 1;
                continue;
            }

            if let Some(limit) = policy.max_files
                && outcome.files.len() >= limit
            {
                outcome.deferred_by_budget = true;
                return Ok(outcome);
            }

            let metadata = fs::metadata(&path).unwrap_or(metadata);
            let canonical = fs::canonicalize(&path).unwrap_or(path);
            let modified_at = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH).into();
            let created_at = metadata.created().ok().map(Into::into);
            let extension = canonical
                .extension()
                .map(|extension| extension.to_string_lossy().to_ascii_lowercase())
                .unwrap_or_default();
            let (volume_id, windows_file_id) = stable_file_identity(&canonical);
            outcome.files.push(DiscoveredFile {
                volume_id,
                windows_file_id,
                canonical_path: canonical.to_string_lossy().into_owned(),
                path_key: path_key(&canonical),
                name: canonical
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                mime_type: mime_type_for_extension(&extension).to_owned(),
                extension,
                size_bytes: metadata.len(),
                created_at,
                modified_at,
                relative_path: canonical
                    .strip_prefix(&canonical_root)
                    .unwrap_or(&canonical)
                    .to_string_lossy()
                    .into_owned(),
            });
        }
    }

    Ok(outcome)
}

#[cfg(windows)]
fn stable_file_identity(path: &Path) -> (String, Option<String>) {
    let mut wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
    wide.push(0);
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            FILE_READ_ATTRIBUTES.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            None,
        )
    };
    let Ok(handle) = handle else {
        return ("unknown".to_owned(), None);
    };
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    let result = unsafe { GetFileInformationByHandle(handle, &mut information) };
    let _ = unsafe { CloseHandle(handle) };
    if result.is_err() {
        return ("unknown".to_owned(), None);
    }
    let file_index = ((information.nFileIndexHigh as u64) << 32) | information.nFileIndexLow as u64;
    (
        format!("vol-{:08x}", information.dwVolumeSerialNumber),
        Some(format!("{file_index:016x}")),
    )
}

#[cfg(not(windows))]
fn stable_file_identity(_path: &Path) -> (String, Option<String>) {
    ("unknown".to_owned(), None)
}

pub fn file_identity_for_path(path: &Path) -> (String, Option<String>) {
    stable_file_identity(path)
}

fn mime_type_for_extension(extension: &str) -> &'static str {
    match extension {
        "pdf" => "application/pdf",
        "docx" | "docm" => {
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
        }
        "xlsx" | "xlsm" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "pptx" | "pptm" => {
            "application/vnd.openxmlformats-officedocument.presentationml.presentation"
        }
        "doc" => "application/msword",
        "xls" => "application/vnd.ms-excel",
        "ppt" => "application/vnd.ms-powerpoint",
        "csv" => "text/csv",
        "tsv" => "text/tab-separated-values",
        "md" | "txt" => "text/plain",
        "html" | "htm" => "text/html",
        "zip" => "application/zip",
        "7z" => "application/x-7z-compressed",
        "rar" => "application/vnd.rar",
        "exe" | "dll" => "application/vnd.microsoft.portable-executable",
        "msi" => "application/x-msi",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "m4a" => "audio/mp4",
        "mp4" => "video/mp4",
        "mkv" => "video/x-matroska",
        "mov" => "video/quicktime",
        "avi" => "video/x-msvideo",
        "webm" => "video/webm",
        "rs" => "text/x-rust",
        "py" => "text/x-python",
        "js" | "jsx" | "mjs" | "cjs" => "text/javascript",
        "ts" | "tsx" => "text/typescript",
        "java" | "kt" | "kts" | "go" | "c" | "cc" | "cpp" | "h" | "hpp" | "cs" | "rb" | "php"
        | "swift" | "scala" | "sh" | "ps1" | "sql" | "json" | "yaml" | "yml" | "toml" | "xml"
        | "css" | "scss" | "vue" | "svelte" => "text/plain",
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "tif" | "tiff" => "image/tiff",
        "bmp" => "image/bmp",
        "webp" => "image/webp",
        _ => "application/octet-stream",
    }
}

pub fn path_key(path: &Path) -> String {
    let value = path
        .to_string_lossy()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_lowercase();
    if let Some(unc_path) = value.strip_prefix("\\\\?\\unc\\") {
        format!("\\\\{unc_path}")
    } else {
        value.strip_prefix("\\\\?\\").unwrap_or(&value).to_owned()
    }
}

fn wildcard_match(pattern: &str, value: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    let starts_with_wildcard = pattern.starts_with('*');
    let ends_with_wildcard = pattern.ends_with('*');
    let needle = pattern.trim_matches('*');
    match (starts_with_wildcard, ends_with_wildcard) {
        (true, true) => value.contains(needle),
        (true, false) => value.ends_with(needle),
        (false, true) => value.starts_with(needle),
        (false, false) => value == needle,
    }
}

#[cfg(windows)]
fn is_hidden_system_or_reparse(metadata: &fs::Metadata) -> bool {
    const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
    const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;
    const FILE_ATTRIBUTE_OFFLINE: u32 = 0x1000;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x40000;
    const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x400000;
    metadata.file_attributes()
        & (FILE_ATTRIBUTE_HIDDEN
            | FILE_ATTRIBUTE_SYSTEM
            | FILE_ATTRIBUTE_REPARSE_POINT
            | FILE_ATTRIBUTE_OFFLINE
            | FILE_ATTRIBUTE_RECALL_ON_OPEN
            | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS)
        != 0
}

#[cfg(not(windows))]
fn is_hidden_system_or_reparse(_metadata: &fs::Metadata) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_is_read_only_and_applies_default_exclusions() {
        let directory = tempfile::tempdir().expect("tempdir");
        let source = directory.path().join("资料.txt");
        fs::write(&source, "拾忆").expect("write fixture");
        fs::create_dir(directory.path().join("node_modules")).expect("create excluded directory");
        fs::write(
            directory.path().join("node_modules").join("ignored.js"),
            "ignored",
        )
        .expect("write excluded fixture");
        fs::write(directory.path().join("~$lock.docx"), "ignored")
            .expect("write temporary fixture");
        fs::create_dir(directory.path().join("AppData")).expect("create hard excluded directory");
        fs::write(
            directory.path().join("AppData").join("secret.txt"),
            "ignored",
        )
        .expect("write hard excluded fixture");
        let before = fs::read(&source).expect("read source before scan");

        let outcome = scan_root(directory.path(), &ScanPolicy::new([])).expect("scan root");

        assert_eq!(outcome.files.len(), 1);
        assert_eq!(outcome.files[0].name, "资料.txt");
        assert_eq!(fs::read(source).expect("read source after scan"), before);
        assert_eq!(outcome.skipped_entries, 3);
    }

    #[test]
    fn protected_paths_are_never_traversed() {
        let directory = tempfile::tempdir().expect("tempdir");
        let protected = directory.path().join("ReminData");
        fs::create_dir(&protected).expect("create protected directory");
        fs::write(protected.join("private.db"), "internal").expect("write protected fixture");

        let outcome = scan_root(
            directory.path(),
            &ScanPolicy::new([protected.to_path_buf()]),
        )
        .expect("scan root");

        assert!(outcome.files.is_empty());
        assert_eq!(outcome.skipped_entries, 1);
    }

    #[test]
    fn protected_path_cannot_be_used_as_the_scan_root() {
        let directory = tempfile::tempdir().expect("tempdir");
        let error = scan_root(
            directory.path(),
            &ScanPolicy::new([directory.path().to_path_buf()]),
        )
        .expect_err("protected root must be rejected");

        assert_eq!(error.code, "ROOT_PROTECTED");
    }

    #[test]
    fn cancelled_scan_stops_at_the_next_atomic_checkpoint() {
        let directory = tempfile::tempdir().expect("tempdir");
        fs::write(directory.path().join("资料.txt"), "拾忆").expect("write fixture");
        let control = ScanControl::default();
        control.cancel();

        let error = scan_root_with_control(directory.path(), &ScanPolicy::new([]), &control)
            .expect_err("cancelled scan must stop");

        assert_eq!(error.code, "SCAN_CANCELLED");
    }

    #[test]
    fn paused_scan_resumes_without_losing_work() {
        let directory = tempfile::tempdir().expect("tempdir");
        fs::write(directory.path().join("资料.txt"), "拾忆").expect("write fixture");
        let root = directory.path().to_path_buf();
        let control = ScanControl::default();
        control.pause();
        let worker_control = control.clone();
        let worker = thread::spawn(move || {
            scan_root_with_control(&root, &ScanPolicy::new([]), &worker_control)
        });
        thread::sleep(Duration::from_millis(100));
        assert!(!worker.is_finished());

        control.resume();
        let outcome = worker.join().expect("join scan").expect("resume scan");

        assert_eq!(outcome.files.len(), 1);
    }
}
