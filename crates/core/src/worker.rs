use std::{
    ffi::OsString,
    io::{BufRead, BufReader, BufWriter, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use uuid::Uuid;

use crate::{AppError, ParseRequest, ParseResult};

/// Strip the Windows long-path (`\\?\` / `\\?\UNC\`) prefix so the worker can
/// open the file: pypdf tolerates it, but Windows.Data.Pdf and PowerShell
/// reject it, which surfaces as PDF_RENDER_FAILED in the OCR chain.
pub fn strip_long_path_prefix(path: &str) -> String {
    if let Some(unc) = path.strip_prefix("\\\\?\\unc\\") {
        format!("\\\\{unc}")
    } else {
        path.strip_prefix("\\\\?\\").unwrap_or(path).to_owned()
    }
}

/// Sidecar 角色：每个角色一个独立 worker 进程，只加载自己的依赖。
/// parse 承载文档解析/导出（无模型）；onnx 承载 embedding/rerank；
/// ocr 承载 RapidOCR；speech 承载 sherpa-onnx ASR/TTS。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WorkerRole {
    Parse,
    Onnx,
    Ocr,
    Speech,
}

impl WorkerRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Parse => "parse",
            Self::Onnx => "onnx",
            Self::Ocr => "ocr",
            Self::Speech => "speech",
        }
    }

    /// 进程空闲多久后由 watchdog 主动终止（下次请求自动重启）。
    /// parse 无模型缓存，与 llama.cpp 的 300 秒休眠一致；其余角色与
    /// worker 内模型会话缓存 60 秒回收保持一致。
    pub fn idle_timeout(self) -> Duration {
        match self {
            Self::Parse => Duration::from_secs(300),
            Self::Onnx | Self::Ocr | Self::Speech => Duration::from_secs(60),
        }
    }

    /// 稳定的实例 UUID（不随进程重启变化），供 RuntimeManager 覆盖注册。
    pub fn instance_uuid(self) -> Uuid {
        let base = match self {
            Self::Parse => 0x6f59_0000_0000_0000_0000_0000_0000_0001u128,
            Self::Onnx => 0x6f59_0000_0000_0000_0000_0000_0000_0002u128,
            Self::Ocr => 0x6f59_0000_0000_0000_0000_0000_0000_0003u128,
            Self::Speech => 0x6f59_0000_0000_0000_0000_0000_0000_0004u128,
        };
        Uuid::from_u128(base)
    }
}

/// watchdog 心跳阈值：worker 每 2 秒写心跳文件，连续超过该时长没有
/// 更新即判定进程失联（Python 主循环卡死等）并受控终止。
const HEARTBEAT_STALE_SECONDS: u64 = 8;

/// 心跳判定：文件缺失或 mtime 距今超过阈值即失联。
fn heartbeat_stale(now_ms: u64, heartbeat_ms: u64) -> bool {
    heartbeat_ms == 0 || now_ms.saturating_sub(heartbeat_ms) / 1000 > HEARTBEAT_STALE_SECONDS
}

/// 空闲判定：距上次请求超过角色空闲阈值即回收（0 表示尚未发起过请求）。
fn idle_expired(now_ms: u64, last_request_ms: u64, idle_timeout_seconds: u64) -> bool {
    last_request_ms > 0 && now_ms.saturating_sub(last_request_ms) / 1000 >= idle_timeout_seconds
}

#[derive(Debug, Clone)]
pub struct WorkerClient {
    launch: WorkerLaunch,
    role: WorkerRole,
    runtime: Arc<Mutex<WorkerRuntime>>,
    active_pid: Arc<AtomicU32>,
    /// 最近一次请求开始时间（Unix 毫秒），watchdog 据此做空闲回收。
    last_request_at_ms: Arc<AtomicU64>,
    /// watchdog 观测到的心跳文件 mtime（Unix 毫秒）。
    heartbeat_at_ms: Arc<AtomicU64>,
    /// watchdog 通过 GetProcessMemoryInfo 观测的真实工作集内存（字节）。
    observed_memory_bytes: Arc<AtomicU64>,
    /// watchdog 通过 GetProcessMemoryInfo 观测的私有提交内存（字节）。
    observed_private_bytes: Arc<AtomicU64>,
    /// 每个活动进程一个 watchdog 线程，进程终止后自行退出。
    supervisor: Arc<Mutex<Option<thread::JoinHandle<()>>>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SidecarSnapshot {
    pub role: &'static str,
    pub pid: u32,
    pub alive: bool,
    pub working_set_bytes: u64,
    pub private_bytes: u64,
    pub last_request_at_ms: u64,
    pub heartbeat_at_ms: u64,
    pub idle_timeout_seconds: u64,
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
pub struct SpeechRecognitionRequest {
    pub model_path: String,
    pub tokens_path: String,
    pub vad_model_path: String,
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    pub threads: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpeechRecognitionResult {
    pub text: String,
    pub sample_rate: u32,
    pub duration_ms: u64,
    pub timestamps: Vec<f32>,
    pub engine: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpeechSynthesisRequest {
    pub model_path: String,
    pub tokens_path: String,
    pub lexicon_path: String,
    pub text: String,
    pub speaker_id: u32,
    pub speed: f32,
    pub threads: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpeechSynthesisResult {
    pub audio_base64: String,
    pub sample_rate: u32,
    pub duration_ms: u64,
    pub speaker_id: u32,
    pub speed: f32,
    pub engine: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeSelfTestResult {
    pub status: String,
    pub engine: String,
    #[serde(default)]
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OcrRecognitionRequest {
    pub model_path: String,
    pub det_model_path: String,
    pub cls_model_path: String,
    pub dictionary_path: String,
    pub image_path: String,
    pub page_no: u32,
    pub threads: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OcrPoint {
    pub x: f32,
    pub y: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OcrLineResult {
    pub page_no: u32,
    pub text: String,
    pub confidence: f32,
    pub polygon: Vec<OcrPoint>,
    pub bbox: crate::BoundingBox,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OcrRecognitionResult {
    pub engine: String,
    pub model_version: String,
    pub page_count: u32,
    pub lines: Vec<OcrLineResult>,
    pub elapsed_ms: u64,
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

fn empty_client(launch: WorkerLaunch, role: WorkerRole) -> WorkerClient {
    WorkerClient {
        launch,
        role,
        runtime: Arc::new(Mutex::new(WorkerRuntime::default())),
        active_pid: Arc::new(AtomicU32::new(0)),
        last_request_at_ms: Arc::new(AtomicU64::new(0)),
        heartbeat_at_ms: Arc::new(AtomicU64::new(0)),
        observed_memory_bytes: Arc::new(AtomicU64::new(0)),
        observed_private_bytes: Arc::new(AtomicU64::new(0)),
        supervisor: Arc::new(Mutex::new(None)),
    }
}

impl WorkerClient {
    pub fn isolated(&self) -> Self {
        empty_client(self.launch.clone(), self.role)
    }

    pub fn with_role(mut self, role: WorkerRole) -> Self {
        self.role = role;
        self
    }

    pub fn role(&self) -> WorkerRole {
        self.role
    }

    pub fn new(python_executable: impl Into<OsString>, worker_root: impl Into<PathBuf>) -> Self {
        empty_client(
            WorkerLaunch::PythonModule {
                python_executable: python_executable.into(),
                worker_root: worker_root.into(),
            },
            WorkerRole::Parse,
        )
    }

    pub fn from_environment(worker_root: impl Into<PathBuf>) -> Self {
        Self::new(
            std::env::var_os("FANFAN_WORKER_PYTHON").unwrap_or_else(|| OsString::from("python")),
            worker_root,
        )
    }

    pub fn from_executable(executable: impl Into<PathBuf>) -> Self {
        let executable = executable.into();
        let working_directory = executable
            .parent()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        empty_client(
            WorkerLaunch::Executable {
                executable,
                working_directory,
            },
            WorkerRole::Parse,
        )
    }

    pub fn parse_document(&self, request: &ParseRequest) -> Result<ParseResult, AppError> {
        self.request_operation("document.parse", request)
    }

    pub fn recognize_ocr(
        &self,
        request: &OcrRecognitionRequest,
    ) -> Result<OcrRecognitionResult, AppError> {
        self.request_operation("ocr.recognize", request)
    }

    pub fn self_test_ocr(
        &self,
        model_path: String,
        det_model_path: String,
        cls_model_path: String,
        dictionary_path: String,
        threads: u32,
    ) -> Result<RuntimeSelfTestResult, AppError> {
        self.request_operation(
            "ocr.self_test",
            &serde_json::json!({
                "model_path": model_path,
                "det_model_path": det_model_path,
                "cls_model_path": cls_model_path,
                "dictionary_path": dictionary_path,
                "threads": threads,
            }),
        )
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

    pub fn recognize_speech(
        &self,
        request: &SpeechRecognitionRequest,
    ) -> Result<SpeechRecognitionResult, AppError> {
        self.request_operation("speech.recognize", request)
    }

    pub fn synthesize_speech(
        &self,
        request: &SpeechSynthesisRequest,
    ) -> Result<SpeechSynthesisResult, AppError> {
        self.request_operation("speech.synthesize", request)
    }

    pub fn self_test_asr(
        &self,
        model_path: String,
        tokens_path: String,
        vad_model_path: String,
        threads: u32,
    ) -> Result<RuntimeSelfTestResult, AppError> {
        #[derive(Serialize)]
        struct Request {
            model_path: String,
            tokens_path: String,
            vad_model_path: String,
            threads: u32,
        }
        self.request_operation(
            "speech.asr_self_test",
            &Request {
                model_path,
                tokens_path,
                vad_model_path,
                threads,
            },
        )
    }

    pub fn self_test_tts(
        &self,
        model_path: String,
        tokens_path: String,
        lexicon_path: String,
        threads: u32,
    ) -> Result<RuntimeSelfTestResult, AppError> {
        #[derive(Serialize)]
        struct Request {
            model_path: String,
            tokens_path: String,
            lexicon_path: String,
            threads: u32,
        }
        self.request_operation(
            "speech.tts_self_test",
            &Request {
                model_path,
                tokens_path,
                lexicon_path,
                threads,
            },
        )
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
        self.last_request_at_ms
            .store(unix_time_ms(), Ordering::Release);
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
            remove_heartbeat_file(self.role, pid);
        }
        if let Ok(mut runtime) = self.runtime.lock() {
            runtime.stop();
        }
    }

    pub fn cancel_active(&self) {
        self.terminate_active_worker();
    }

    /// watchdog 最近一次观测快照；进程未启动时返回 None。
    pub fn supervisor_snapshot(&self) -> Option<SidecarSnapshot> {
        let pid = self.active_pid.load(Ordering::Acquire);
        if pid == 0 {
            return None;
        }
        Some(SidecarSnapshot {
            role: self.role.as_str(),
            pid,
            alive: process_alive(pid),
            working_set_bytes: self.observed_memory_bytes.load(Ordering::Acquire),
            private_bytes: self.observed_private_bytes.load(Ordering::Acquire),
            last_request_at_ms: self.last_request_at_ms.load(Ordering::Acquire),
            heartbeat_at_ms: self.heartbeat_at_ms.load(Ordering::Acquire),
            idle_timeout_seconds: self.role.idle_timeout().as_secs(),
        })
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
                    "本地文档处理组件不可用，请修复或重新安装翻翻",
                    false,
                ))
            }
            _ => Ok(()),
        }
    }

    fn start_process(&self) -> Result<WorkerProcess, AppError> {
        // 心跳文件按角色固定命名：每个角色同时只有一个进程，spawn 前清掉
        // 上次崩溃残留的旧文件，避免 watchdog 误判新进程仍然存活。
        let heartbeat_file = heartbeat_file_path(self.role);
        let _ = std::fs::remove_file(&heartbeat_file);
        let mut command = match &self.launch {
            WorkerLaunch::PythonModule {
                python_executable,
                worker_root,
            } => {
                let mut command = Command::new(python_executable);
                command
                    .args(["-m", "fanfan_worker"])
                    .arg("--role")
                    .arg(self.role.as_str())
                    .arg("--heartbeat-file")
                    .arg(&heartbeat_file)
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
                command
                    .arg("--role")
                    .arg(self.role.as_str())
                    .arg("--heartbeat-file")
                    .arg(&heartbeat_file)
                    .current_dir(working_directory);
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
        // 加入全局 Job Object：app 进程退出时（含崩溃）级联回收全部 sidecar，
        // 并统一以低于正常的优先级运行，避免后台索引与前台交互抢 CPU。
        if let Err(error) = assign_to_sidecar_job(child.id()) {
            eprintln!(
                "[worker] 角色 {} pid {} 加入 Job Object 失败: {}",
                self.role.as_str(),
                child.id(),
                error
            );
        }
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| AppError::new("WORKER_IO_FAILED", "无法打开Worker标准输入", true))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AppError::new("WORKER_IO_FAILED", "无法打开Worker标准输出", true))?;
        let pid = child.id();
        self.spawn_watchdog(pid, heartbeat_file);
        Ok(WorkerProcess {
            child,
            stdin: BufWriter::new(stdin),
            stdout: BufReader::new(stdout),
        })
    }

    /// 每 2 秒执行一次进程级心跳观测：
    /// - 意外退出（未被主动终止）→ 清理 active_pid，下次请求自动重启；
    /// - 心跳文件超时未更新（Python 主循环卡死）→ 受控终止；
    /// - 空闲超过角色阈值 → 主动终止（完整休眠，下次请求自动重启）；
    /// - 通过 GetProcessMemoryInfo 观测真实内存，供运行时状态展示。
    fn spawn_watchdog(&self, pid: u32, heartbeat_file: PathBuf) {
        let role = self.role;
        let active_pid = Arc::clone(&self.active_pid);
        let last_request_at_ms = Arc::clone(&self.last_request_at_ms);
        let heartbeat_at_ms = Arc::clone(&self.heartbeat_at_ms);
        let observed_memory_bytes = Arc::clone(&self.observed_memory_bytes);
        let observed_private_bytes = Arc::clone(&self.observed_private_bytes);
        let handle = thread::Builder::new()
            .name(format!("sidecar-watchdog-{}", role.as_str()))
            .spawn(move || {
                let idle_timeout = role.idle_timeout().as_secs();
                loop {
                    thread::sleep(Duration::from_millis(2000));
                    if active_pid.load(Ordering::Acquire) != pid {
                        return; // 已被主动终止或替换
                    }
                    if let Some(exit) = try_wait(pid) {
                        eprintln!(
                            "[worker] sidecar {} (pid {}) 意外退出，状态码 {:?}",
                            role.as_str(),
                            pid,
                            exit
                        );
                        active_pid.store(0, Ordering::Release);
                        remove_heartbeat_file(role, pid);
                        return;
                    }
                    let now_ms = unix_time_ms();
                    let heartbeat_ms = heartbeat_file_mtime_ms(&heartbeat_file);
                    if heartbeat_ms > 0 {
                        heartbeat_at_ms.store(heartbeat_ms, Ordering::Release);
                    }
                    if heartbeat_stale(now_ms, heartbeat_ms) {
                        eprintln!(
                            "[worker] sidecar {} (pid {}) 心跳丢失超过 {}s，判定失联并终止",
                            role.as_str(),
                            pid,
                            HEARTBEAT_STALE_SECONDS
                        );
                        active_pid.store(0, Ordering::Release);
                        terminate_process_by_id(pid);
                        remove_heartbeat_file(role, pid);
                        return;
                    }
                    let last_request = last_request_at_ms.load(Ordering::Acquire);
                    if idle_expired(now_ms, last_request, idle_timeout) {
                        eprintln!(
                            "[worker] sidecar {} (pid {}) 空闲超过 {}s，休眠回收",
                            role.as_str(),
                            pid,
                            idle_timeout
                        );
                        active_pid.store(0, Ordering::Release);
                        terminate_process_by_id(pid);
                        remove_heartbeat_file(role, pid);
                        return;
                    }
                    if let Some((working_set, private_bytes)) = process_memory(pid) {
                        observed_memory_bytes.store(working_set, Ordering::Release);
                        observed_private_bytes.store(private_bytes, Ordering::Release);
                    }
                }
            });
        if let Ok(handle) = handle {
            if let Ok(mut supervisor) = self.supervisor.lock() {
                *supervisor = Some(handle);
            }
        }
    }
}

fn operation_timeout(operation: &str) -> Duration {
    match operation {
        // Must stay above the worker-side OCR budget (ocr.py caps at 270s)
        // so a long scan-heavy parse is never killed mid-OCR.
        "document.parse" => Duration::from_secs(360),
        "embedding.encode" | "export.write" | "speech.synthesize" => Duration::from_secs(120),
        "speech.recognize"
        | "speech.asr_self_test"
        | "speech.tts_self_test"
        | "ocr.recognize"
        | "ocr.self_test" => Duration::from_secs(90),
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

fn unix_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

/// 心跳文件按角色固定命名（每个角色同一时刻只有一个进程；
/// spawn 前会删除上次崩溃的残留文件，避免误判存活）。
fn heartbeat_file_path(role: WorkerRole) -> PathBuf {
    std::env::temp_dir().join(format!("fanfan-{}.heartbeat", role.as_str()))
}

fn remove_heartbeat_file(role: WorkerRole, _pid: u32) {
    let _ = std::fs::remove_file(heartbeat_file_path(role));
}

/// 心跳文件 mtime（Unix 毫秒）；文件不存在返回 0。
fn heartbeat_file_mtime_ms(path: &Path) -> u64 {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(windows)]
fn try_wait(pid: u32) -> Option<u32> {
    use windows::Win32::{
        Foundation::{CloseHandle, WAIT_OBJECT_0},
        System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, WaitForSingleObject},
    };

    let handle = match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
        Ok(handle) => handle,
        Err(_) => return Some(0), // 打开失败视为已退出
    };
    let result = unsafe { WaitForSingleObject(handle, 0) };
    let _ = unsafe { CloseHandle(handle) };
    (result == WAIT_OBJECT_0).then_some(0)
}

#[cfg(not(windows))]
fn try_wait(_pid: u32) -> Option<u32> {
    None
}

#[cfg(windows)]
fn process_alive(pid: u32) -> bool {
    try_wait(pid).is_none()
}

#[cfg(not(windows))]
fn process_alive(_pid: u32) -> bool {
    false
}

/// 通过 GetProcessMemoryInfo 观测真实内存（工作集 + 私有提交量）。
#[cfg(windows)]
fn process_memory(pid: u32) -> Option<(u64, u64)> {
    use std::mem::size_of;
    use windows::Win32::{
        Foundation::CloseHandle,
        System::{
            ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX},
            Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
        },
    };

    let handle = match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
        Ok(handle) => handle,
        Err(_) => return None,
    };
    let mut counters = PROCESS_MEMORY_COUNTERS_EX::default();
    // windows crate 的包装只暴露 PROCESS_MEMORY_COUNTERS 类型，但底层按 cb
    // 大小区分：EX 版布局为兼容超集，强转传指针是标准做法（官方文档允许）。
    let ok = unsafe {
        GetProcessMemoryInfo(
            handle,
            &mut counters as *mut PROCESS_MEMORY_COUNTERS_EX as *mut PROCESS_MEMORY_COUNTERS,
            size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        )
    };
    let _ = unsafe { CloseHandle(handle) };
    ok.is_ok()
        .then_some((counters.WorkingSetSize as u64, counters.PrivateUsage as u64))
}

#[cfg(not(windows))]
fn process_memory(_pid: u32) -> Option<(u64, u64)> {
    None
}

/// 全局 Job Object：KILL_ON_JOB_CLOSE 让所有 sidecar 随 app 进程树级联回收
/// （包括 app 崩溃/被杀时），进程级优先级统一低于正常。
#[cfg(windows)]
fn assign_to_sidecar_job(pid: u32) -> Result<(), AppError> {
    use windows::Win32::{
        Foundation::{CloseHandle, HANDLE},
        System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject,
            JobObjectBasicLimitInformation, JobObjectExtendedLimitInformation,
            JOBOBJECT_BASIC_LIMIT_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        },
        System::Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE},
    };

    // 进程级 Job 单例：app 存活期间始终持有 handle（进程退出时由 OS 回收），
    // 因此整个进程生命周期内只有一个 Job 实例，无需释放。Windows 句柄在
    // 进程内可安全跨线程使用，仅此处持有并复用，手动实现 Send/Sync 是安全的。
    struct JobHandle(HANDLE);
    unsafe impl Send for JobHandle {}
    unsafe impl Sync for JobHandle {}
    static JOB: std::sync::Mutex<Option<JobHandle>> = std::sync::Mutex::new(None);
    let mut job_guard = JOB
        .lock()
        .map_err(|_| AppError::new("JOB_OBJECT_UNAVAILABLE", "Job Object 锁已损坏", true))?;
    let job = if let Some(JobHandle(job)) = *job_guard {
        job
    } else {
        let job = unsafe { CreateJobObjectW(None, None) }
            .map_err(|error| AppError::new("JOB_OBJECT_CREATE_FAILED", error.to_string(), true))?;
        let mut extended = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        extended.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &extended as *const _ as *const _,
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        }
        .map_err(|error| AppError::new("JOB_OBJECT_CONFIGURE_FAILED", error.to_string(), true))?;
        let mut basic = JOBOBJECT_BASIC_LIMIT_INFORMATION::default();
        const BELOW_NORMAL_PRIORITY_CLASS: u32 = 0x0000_4000;
        basic.PriorityClass = BELOW_NORMAL_PRIORITY_CLASS;
        unsafe {
            SetInformationJobObject(
                job,
                JobObjectBasicLimitInformation,
                &basic as *const _ as *const _,
                size_of::<JOBOBJECT_BASIC_LIMIT_INFORMATION>() as u32,
            )
        }
        .map_err(|error| AppError::new("JOB_OBJECT_CONFIGURE_FAILED", error.to_string(), true))?;
        *job_guard = Some(JobHandle(job));
        job
    };

    let process = match unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, false, pid) } {
        Ok(process) => process,
        Err(_) => {
            return Err(AppError::new(
                "JOB_OBJECT_OPEN_PROCESS_FAILED",
                "无法打开sidecar进程句柄",
                true,
            ))
        }
    };
    let result = unsafe { AssignProcessToJobObject(job, process) };
    let _ = unsafe { CloseHandle(process) };
    result.map_err(|error| AppError::new("JOB_OBJECT_ASSIGN_FAILED", error.to_string(), true))
}

#[cfg(not(windows))]
fn assign_to_sidecar_job(_pid: u32) -> Result<(), AppError> {
    Ok(())
}

impl WorkerProcess {
    fn request(&mut self, request_json: &[u8]) -> Result<String, AppError> {
        self.stdin
            .write_all(request_json)
            .and_then(|_| self.stdin.flush())
            .map_err(|error| AppError::new("WORKER_IO_FAILED", error.to_string(), true))?;
        read_worker_protocol_response(&mut self.stdout)
    }
}

fn read_worker_protocol_response(reader: &mut impl BufRead) -> Result<String, AppError> {
    const MAX_NOISE_LINES: usize = 64;
    const MAX_NOISE_BYTES: usize = 128 * 1024;
    let mut ignored_bytes = 0_usize;
    for _ in 0..MAX_NOISE_LINES {
        let mut response = String::new();
        let bytes = reader
            .read_line(&mut response)
            .map_err(|error| AppError::new("WORKER_IO_FAILED", error.to_string(), true))?;
        if bytes == 0 {
            return Err(AppError::new(
                "WORKER_RESPONSE_INVALID",
                "Worker进程已经退出且没有返回结构化结果",
                true,
            ));
        }
        let is_protocol_response = serde_json::from_str::<serde_json::Value>(&response)
            .ok()
            .and_then(|value| value.as_object().cloned())
            .is_some_and(|value| {
                value.get("request_id").is_some()
                    && value.get("ok").is_some_and(serde_json::Value::is_boolean)
                    && (value.contains_key("result") || value.contains_key("error"))
            });
        if is_protocol_response {
            return Ok(response);
        }
        ignored_bytes = ignored_bytes.saturating_add(bytes);
        if ignored_bytes > MAX_NOISE_BYTES {
            break;
        }
    }
    Err(AppError::new(
        "WORKER_PROTOCOL_NOISE_LIMIT",
        "Worker运行库向协议通道写入了过多非结构化输出，已停止本次操作",
        true,
    ))
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
    fn native_library_stdout_noise_does_not_replace_the_json_response() {
        let mut input = std::io::Cursor::new(
            b"native runtime banner\nprogress: 100%\n{\"request_id\":\"018f0000-0000-7000-8000-000000000001\",\"ok\":true,\"result\":{},\"error\":null}\n",
        );
        let response = read_worker_protocol_response(&mut input).expect("skip native noise");
        assert!(response.contains("\"ok\":true"));
    }

    #[test]
    fn sidecar_roles_have_stable_instances_and_idle_timeouts() {
        assert_eq!(WorkerRole::Parse.as_str(), "parse");
        assert_eq!(WorkerRole::Onnx.as_str(), "onnx");
        assert_eq!(WorkerRole::Ocr.as_str(), "ocr");
        assert_eq!(WorkerRole::Speech.as_str(), "speech");
        assert_eq!(WorkerRole::Parse.idle_timeout(), Duration::from_secs(300));
        assert_eq!(WorkerRole::Onnx.idle_timeout(), Duration::from_secs(60));
        // 实例 UUID 跨调用稳定（RuntimeManager 覆盖注册依赖此性质）。
        assert_eq!(WorkerRole::Onnx.instance_uuid(), WorkerRole::Onnx.instance_uuid());
        assert_ne!(WorkerRole::Onnx.instance_uuid(), WorkerRole::Ocr.instance_uuid());
    }

    #[test]
    fn heartbeat_stale_requires_fresh_file() {
        let now = 1_000_000;
        assert!(heartbeat_stale(now, 0), "缺失心跳文件视为失联");
        assert!(heartbeat_stale(now, now - 9_000), "超过 8 秒未更新视为失联");
        assert!(!heartbeat_stale(now, now - 4_000), "4 秒内的心跳视为存活");
        assert!(!heartbeat_stale(now, now), "刚更新的心跳视为存活");
    }

    #[test]
    fn idle_expired_respects_timeout_and_unused_state() {
        let now = 1_000_000;
        assert!(!idle_expired(now, 0, 60), "从未请求过不回收");
        assert!(!idle_expired(now, now - 30_000, 60), "30 秒未超 60 秒阈值");
        assert!(idle_expired(now, now - 61_000, 60), "超过阈值回收");
        assert!(idle_expired(now, now - 400_000, 300), "parse 300 秒阈值");
        assert!(!idle_expired(now, now - 100_000, 300));
    }

    #[test]
    fn missing_packaged_worker_fails_before_process_start() {
        let client = WorkerClient::from_executable("missing-fanfan-worker.exe");
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
            ocr_runtime: None,
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
            ocr_runtime: None,
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
