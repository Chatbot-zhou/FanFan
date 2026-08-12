use std::{
    ffi::OsString,
    io::{BufRead, BufReader, BufWriter, Write},
    path::PathBuf,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::{AppError, ParseRequest, ParseResult};

#[derive(Debug, Clone)]
pub struct WorkerClient {
    launch: WorkerLaunch,
    runtime: Arc<Mutex<WorkerRuntime>>,
    active_pid: Arc<AtomicU32>,
}

#[derive(Debug, Clone)]
enum WorkerLaunch {
    PythonModule {
        python_executable: OsString,
        worker_root: PathBuf,
    },
    Executable {
        executable: PathBuf,
        working_directory: PathBuf,
    },
}

#[derive(Debug, Default)]
struct WorkerRuntime {
    process: Option<WorkerProcess>,
}

#[derive(Debug)]
struct WorkerProcess {
    child: Child,
    stdin: BufWriter<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

#[derive(Debug, Serialize)]
struct WorkerRequestEnvelope<'a, T> {
    request_id: String,
    operation: &'static str,
    payload: &'a T,
}

#[derive(Debug, Deserialize)]
struct WorkerResponseEnvelope<T> {
    ok: bool,
    result: Option<T>,
    error: Option<AppError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingRequest {
    pub model_path: String,
    pub tokenizer_path: Option<String>,
    pub texts: Vec<String>,
    pub max_length: u32,
    pub threads: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingResponse {
    pub vectors: Vec<Vec<f32>>,
    pub dimension: u32,
    pub model_path: String,
    pub tokenizer_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RerankRequest {
    pub model_path: String,
    pub tokenizer_path: Option<String>,
    pub query: String,
    pub documents: Vec<String>,
    pub max_length: u32,
    pub threads: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RerankResponse {
    pub scores: Vec<f32>,
    pub model_path: String,
    pub tokenizer_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportTableRequest {
    pub target_path: String,
    pub format: String,
    pub headers: Vec<String>,
    pub rows: Vec<Vec<serde_json::Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportResult {
    pub target_path: String,
    pub format: String,
    pub row_count: u64,
    pub size_bytes: u64,
    pub sha256: String,
}

impl WorkerClient {
    pub fn isolated(&self) -> Self {
        Self {
            launch: self.launch.clone(),
            runtime: Arc::new(Mutex::new(WorkerRuntime::default())),
            active_pid: Arc::new(AtomicU32::new(0)),
        }
    }

    pub fn new(python_executable: impl Into<OsString>, worker_root: impl Into<PathBuf>) -> Self {
        Self {
            launch: WorkerLaunch::PythonModule {
                python_executable: python_executable.into(),
                worker_root: worker_root.into(),
            },
            runtime: Arc::new(Mutex::new(WorkerRuntime::default())),
            active_pid: Arc::new(AtomicU32::new(0)),
        }
    }

    pub fn from_environment(worker_root: impl Into<PathBuf>) -> Self {
        Self::new(
            std::env::var_os("REMIN_WORKER_PYTHON").unwrap_or_else(|| OsString::from("python")),
            worker_root,
        )
    }

    pub fn from_executable(executable: impl Into<PathBuf>) -> Self {
        let executable = executable.into();
        let working_directory = executable
            .parent()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        Self {
            launch: WorkerLaunch::Executable {
                executable,
                working_directory,
            },
            runtime: Arc::new(Mutex::new(WorkerRuntime::default())),
            active_pid: Arc::new(AtomicU32::new(0)),
        }
    }

    pub fn parse_document(&self, request: &ParseRequest) -> Result<ParseResult, AppError> {
        self.request_operation("document.parse", request)
    }

    pub fn encode_embeddings(
        &self,
        request: &EmbeddingRequest,
    ) -> Result<EmbeddingResponse, AppError> {
        self.request_operation("embedding.encode", request)
    }

    pub fn rerank(&self, request: &RerankRequest) -> Result<RerankResponse, AppError> {
        self.request_operation("rerank.score", request)
    }

    pub fn export_table(&self, request: &ExportTableRequest) -> Result<ExportResult, AppError> {
        self.request_operation("export.write", request)
    }

    fn request_operation<P, R>(&self, operation: &'static str, payload: &P) -> Result<R, AppError>
    where
        P: Serialize,
        R: DeserializeOwned + Send + 'static,
    {
        self.validate_launch_target()?;
        let envelope = WorkerRequestEnvelope {
            request_id: uuid::Uuid::now_v7().to_string(),
            operation,
            payload,
        };
        let mut request_json = serde_json::to_vec(&envelope)
            .map_err(|error| AppError::new("WORKER_REQUEST_INVALID", error.to_string(), false))?;
        request_json.push(b'\n');

        let max_attempts = if operation == "export.write" { 1 } else { 2 };
        let timeout = operation_timeout(operation);
        let mut last_error = None;
        for _ in 0..max_attempts {
            let client = self.clone();
            let request = request_json.clone();
            let (sender, receiver) = mpsc::sync_channel(1);
            thread::spawn(move || {
                let _ = sender.send(client.request_serialized(&request));
            });
            match receiver.recv_timeout(timeout) {
                Ok(result) => match result {
                    Ok(value) => return Ok(value),
                    Err(error) if error.retryable && max_attempts > 1 => {
                        last_error = Some(error);
                        self.terminate_active_worker();
                    }
                    Err(error) => return Err(error),
                },
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    self.terminate_active_worker();
                    last_error = Some(AppError::new(
                        "WORKER_OPERATION_TIMEOUT",
                        format!(
                            "Worker操作 {operation} 超过 {:?}，已终止并准备受控重启",
                            timeout
                        ),
                        true,
                    ));
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    last_error = Some(AppError::new(
                        "WORKER_CHANNEL_DISCONNECTED",
                        "Worker监督通道意外断开",
                        true,
                    ));
                }
            }
        }
        Err(last_error.unwrap_or_else(|| {
            AppError::new("WORKER_OPERATION_FAILED", "Worker未能完成操作", true)
        }))
    }

    fn request_serialized<R>(&self, request_json: &[u8]) -> Result<R, AppError>
    where
        R: DeserializeOwned,
    {
        let mut runtime = self
            .runtime
            .lock()
            .map_err(|_| AppError::new("WORKER_IO_FAILED", "Worker运行锁已经损坏", true))?;
        if runtime.process.is_none() {
            runtime.process = Some(self.start_process()?);
        }
        let response_line = match runtime
            .process
            .as_mut()
            .expect("worker process initialized")
            .request(request_json)
        {
            Ok(response) => response,
            Err(error) => {
                runtime.stop();
                self.active_pid.store(0, Ordering::Release);
                return Err(error);
            }
        };
        let response = decode_worker_response(&response_line)?;
        if let Some(result) = response.result {
            return Ok(result);
        }
        Err(response.error.unwrap_or_else(|| {
            AppError::new(
                "WORKER_OPERATION_FAILED",
                if response.ok {
                    "Worker返回成功但缺少操作结果"
                } else {
                    "Worker未能完成解析操作"
                },
                true,
            )
        }))
    }

    fn terminate_active_worker(&self) {
        let pid = self.active_pid.swap(0, Ordering::AcqRel);
        if pid != 0 {
            terminate_process_by_id(pid);
        }
    }

    pub fn cancel_active(&self) {
        self.terminate_active_worker();
        if let Ok(mut runtime) = self.runtime.lock() {
            runtime.stop();
        }
    }

    fn validate_launch_target(&self) -> Result<(), AppError> {
        match &self.launch {
            WorkerLaunch::PythonModule { worker_root, .. } if !worker_root.is_dir() => {
                Err(AppError::new(
                    "WORKER_ROOT_UNAVAILABLE",
                    "本地文档处理组件目录不存在",
                    false,
                ))
            }
            WorkerLaunch::Executable { executable, .. } if !executable.is_file() => {
                Err(AppError::new(
                    "WORKER_EXECUTABLE_UNAVAILABLE",
                    "本地文档处理组件不可用，请修复或重新安装拾忆",
                    false,
                ))
            }
            _ => Ok(()),
        }
    }

    fn start_process(&self) -> Result<WorkerProcess, AppError> {
        let mut command = match &self.launch {
            WorkerLaunch::PythonModule {
                python_executable,
                worker_root,
            } => {
                let mut command = Command::new(python_executable);
                command
                    .args(["-m", "remin_worker"])
                    .current_dir(worker_root)
                    .env("PYTHONUTF8", "1")
                    .env("PYTHONIOENCODING", "utf-8");
                command
            }
            WorkerLaunch::Executable {
                executable,
                working_directory,
            } => {
                let mut command = Command::new(executable);
                command.current_dir(working_directory);
                command
            }
        };
        hide_child_console(&mut command);
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| AppError::new("WORKER_START_FAILED", error.to_string(), true))?;
        self.active_pid.store(child.id(), Ordering::Release);
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| AppError::new("WORKER_IO_FAILED", "无法打开Worker标准输入", true))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AppError::new("WORKER_IO_FAILED", "无法打开Worker标准输出", true))?;
        Ok(WorkerProcess {
            child,
            stdin: BufWriter::new(stdin),
            stdout: BufReader::new(stdout),
        })
    }
}

fn operation_timeout(operation: &str) -> Duration {
    match operation {
        // Must stay above the worker-side OCR budget (ocr.py caps at 270s)
        // so a long scan-heavy parse is never killed mid-OCR.
        "document.parse" => Duration::from_secs(360),
        "embedding.encode" | "export.write" => Duration::from_secs(120),
        _ => Duration::from_secs(60),
    }
}

#[cfg(windows)]
fn terminate_process_by_id(pid: u32) {
    use windows::Win32::{
        Foundation::CloseHandle,
        System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess},
    };

    if let Ok(process) = unsafe { OpenProcess(PROCESS_TERMINATE, false, pid) } {
        let _ = unsafe { TerminateProcess(process, 1) };
        let _ = unsafe { CloseHandle(process) };
    }
}

#[cfg(not(windows))]
fn terminate_process_by_id(_pid: u32) {}

#[cfg(windows)]
fn hide_child_console(command: &mut Command) {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn hide_child_console(_command: &mut Command) {}

impl WorkerProcess {
    fn request(&mut self, request_json: &[u8]) -> Result<String, AppError> {
        self.stdin
            .write_all(request_json)
            .and_then(|_| self.stdin.flush())
            .map_err(|error| AppError::new("WORKER_IO_FAILED", error.to_string(), true))?;
        let mut response = String::new();
        let bytes = self
            .stdout
            .read_line(&mut response)
            .map_err(|error| AppError::new("WORKER_IO_FAILED", error.to_string(), true))?;
        if bytes == 0 {
            return Err(AppError::new(
                "WORKER_RESPONSE_INVALID",
                "Worker进程已经退出且没有返回结果",
                true,
            ));
        }
        Ok(response)
    }
}

impl WorkerRuntime {
    fn stop(&mut self) {
        if let Some(mut process) = self.process.take() {
            let _ = process.child.kill();
            let _ = process.child.wait();
        }
    }
}

impl Drop for WorkerRuntime {
    fn drop(&mut self) {
        self.stop();
    }
}

fn decode_worker_response<T: DeserializeOwned>(
    response_line: &str,
) -> Result<WorkerResponseEnvelope<T>, AppError> {
    serde_json::from_str(response_line)
        .map_err(|error| AppError::new("WORKER_RESPONSE_INVALID", error.to_string(), false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ParseOutcome, ParseRequest};

    #[test]
    fn python_parse_response_contract_deserializes() {
        let response: WorkerResponseEnvelope<ParseResult> = decode_worker_response(
            r#"{
                "request_id":"018f0000-0000-7000-8000-000000000001",
                "ok":true,
                "result":{
                    "revision_id":"018f0000-0000-7000-8000-000000000003",
                    "status":"parsed",
                    "parser_name":"stdlib-text",
                    "parser_version":"0.1.0",
                    "nodes":[{
                        "node_id":"018f0000-0000-7000-8000-000000000004",
                        "parent_id":null,
                        "ordinal":1,
                        "node_type":"paragraph",
                        "text":"归航计划",
                        "table_data":null,
                        "locator":{"kind":"text","page_no":null,"slide_no":null,"sheet_name":null,"cell_range":null,"paragraph_no":null,"line_start":1,"line_end":1,"shape_no":null,"bbox":null,"heading_path":[]},
                        "heading_path":[]
                    }],
                    "warnings":[],
                    "metrics":{"page_count":0,"node_count":1,"character_count":4,"ocr_page_count":0,"elapsed_ms":1},
                    "error":null
                },
                "error":null
            }"#,
        )
        .expect("decode Python response");

        assert!(response.ok);
        let result = response.result.expect("parse result");
        assert_eq!(result.status, crate::ParseOutcome::Parsed);
        assert_eq!(result.nodes[0].locator.line_start, Some(1));
    }

    #[test]
    fn missing_packaged_worker_fails_before_process_start() {
        let client = WorkerClient::from_executable("missing-remin-worker.exe");
        let request = ParseRequest {
            job_id: uuid::Uuid::now_v7(),
            file_id: uuid::Uuid::now_v7(),
            revision_id: uuid::Uuid::now_v7(),
            source_path: "unused.md".into(),
            format: "md".into(),
            ocr_policy: "auto".into(),
            language_hints: vec!["zh".into()],
            max_pages: None,
            asset_cache_dir: None,
            parser_version: "0.1.0".into(),
        };

        let error = client
            .parse_document(&request)
            .expect_err("missing packaged worker must fail");
        assert_eq!(error.code, "WORKER_EXECUTABLE_UNAVAILABLE");
    }

    #[test]
    #[ignore = "requires the local Python worker runtime"]
    fn cross_process_worker_parses_a_real_chinese_fixture() {
        let repository_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let client = WorkerClient::from_environment(repository_root.join("services/worker"));
        let request = ParseRequest {
            job_id: uuid::Uuid::now_v7(),
            file_id: uuid::Uuid::now_v7(),
            revision_id: uuid::Uuid::now_v7(),
            source_path: repository_root
                .join("tests/fixtures/corpus/05-项目说明.md")
                .to_string_lossy()
                .into_owned(),
            format: "md".into(),
            ocr_policy: "auto".into(),
            language_hints: vec!["zh".into()],
            max_pages: None,
            asset_cache_dir: None,
            parser_version: "0.1.0".into(),
        };
        let result = client
            .parse_document(&request)
            .expect("parse fixture through Python subprocess");
        let first_process_id = client
            .runtime
            .lock()
            .expect("runtime lock")
            .process
            .as_ref()
            .expect("worker process")
            .child
            .id();
        let mut second_request = request.clone();
        second_request.job_id = uuid::Uuid::now_v7();
        second_request.revision_id = uuid::Uuid::now_v7();
        client
            .parse_document(&second_request)
            .expect("reuse worker process for second parse");
        let second_process_id = client
            .runtime
            .lock()
            .expect("runtime lock")
            .process
            .as_ref()
            .expect("worker process")
            .child
            .id();

        assert_eq!(result.status, ParseOutcome::Parsed);
        assert!(
            result
                .nodes
                .iter()
                .filter_map(|node| node.text.as_deref())
                .any(|text| text.contains("GH-2025-017"))
        );
        assert_eq!(first_process_id, second_process_id);
    }
}
