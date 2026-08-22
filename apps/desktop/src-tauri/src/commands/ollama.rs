//! Ollama 状态查询与启动命令（阶段5，供前端安装引导 / 设置页使用）。
//!
//! 职责：
//! - `ollama_status_get`：只读探测三态（未装 / 已装未运行 / 已就绪），返回版本与启动状态。
//! - `ollama_start`：触发前台启动请求；内部做防重复启动（端口已监听则直接复用），
//!   后台拉起 `ollama serve` 并轮询健康，全程不阻塞也不卡死 UI；启动失败仅返回
//!   结构化状态，绝不 panic / 崩溃；未安装时只引导安装，不静默安装、不下载安装包。
//!
//! 与 `lib.rs::detect_ollama_runtime` 保持同一「三态 + 上报 `ollama:state`」契约。

use serde::Serialize;
use tauri::{AppHandle, Emitter};

use fanfan_core::ollama::{
    OLLAMA_START_TIMEOUT, OllamaProbe, OllamaStatus, ensure_running, probe_ollama, probe_status,
};

/// 面向前端的 Ollama 三态快照。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct OllamaStatusSnapshot {
    /// `ready` / `installed_not_running` / `not_installed`。
    pub status: String,
    /// 就绪时的服务版本；未就绪为空串。
    pub version: String,
    /// 是否处于后台启动中（已装未运行且已发起启动）。
    pub starting: bool,
    /// 最近一次启动失败的结构化错误码；无失败为 `None`。
    pub error_code: Option<String>,
}

impl OllamaStatusSnapshot {
    fn from_probe(probe: &OllamaProbe, starting: bool, error_code: Option<String>) -> Self {
        let status = match probe_status(probe) {
            OllamaStatus::Ready => "ready",
            OllamaStatus::InstalledNotRunning => "installed_not_running",
            OllamaStatus::NotInstalled => "not_installed",
        };
        Self {
            status: status.into(),
            version: probe
                .version
                .as_ref()
                .map(|v| v.version.clone())
                .unwrap_or_default(),
            starting,
            error_code,
        }
    }
}

/// 只读探测 Ollama 当前三态。
#[tauri::command(async)]
pub fn ollama_status_get() -> OllamaStatusSnapshot {
    OllamaStatusSnapshot::from_probe(&probe_ollama(), false, None)
}

/// 请求启动本机 Ollama。
///
/// - 已就绪：直接返回 `ready`。
/// - 已装未运行：返回 `installed_not_running + starting=true`，并在后台拉起服务、
///   轮询健康；就绪/失败时回发 `ollama:state` 事件，前端据此刷新。
/// - 未安装：返回 `not_installed`（引导安装，不做任何自动安装动作）。
#[tauri::command(async)]
pub fn ollama_start(app: AppHandle) -> OllamaStatusSnapshot {
    let probe = probe_ollama();
    match probe_status(&probe) {
        OllamaStatus::Ready => OllamaStatusSnapshot::from_probe(&probe, false, None),
        OllamaStatus::NotInstalled => OllamaStatusSnapshot::from_probe(&probe, false, None),
        OllamaStatus::InstalledNotRunning => {
            start_in_background(app);
            OllamaStatusSnapshot::from_probe(&probe, true, None)
        }
    }
}

/// 请求关闭本机 Ollama 服务（终止 `ollama serve` / `ollama.exe` 进程）。
/// 仅在已就绪时有意义；终止后重新探测并回发 `ollama:state` 事件。
///
/// 说明：
/// - Ollama 不提供"关闭服务"的 HTTP 端点，因此只能走进程级优雅退出。
/// - Windows 版 Ollama 的托盘程序 `ollama app.exe` 会作为看护进程，在服务
///   进程被终止后自动重新拉起 `ollama serve`，必须一并处理，否则"关不掉"。
/// - 关闭分两阶段：先向托盘发送优雅关闭信号让其自行停止服务；等待 2 秒
///   仍未退出时才强制终止（兜底），尽量不硬杀后台。
#[tauri::command(async)]
pub fn ollama_stop(app: AppHandle) -> OllamaStatusSnapshot {
    // 第一阶段（优雅）：先向看护进程（托盘 `ollama app.exe`）发送关闭信号，
    // 让其自行停止服务并退出，避免遗留未清理的模型驻留 / 下载状态。
    let _ = std::process::Command::new("taskkill")
        .args(["/IM", "ollama app.exe", "/T"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    // 给托盘 2 秒自行清理并停止服务。
    std::thread::sleep(std::time::Duration::from_millis(2000));
    // 第二阶段（兜底）：服务仍存活时强制终止看护进程与服务进程
    // （先看护后服务，避免服务被看护进程重新拉起）。
    if probe_ollama().running {
        for image in ["ollama app.exe", "ollama.exe"] {
            let _ = std::process::Command::new("taskkill")
                .args(["/IM", image, "/T", "/F"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
    }
    // 轮询确认端口真正释放（Windows TCP TIME_WAIT 可能导致短暂可达）且服务
    // 未被重新拉起；最多等 3 秒，每 300ms 探测一次。
    let mut probe = probe_ollama();
    for _ in 0..10 {
        if !probe.running {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
        probe = probe_ollama();
    }
    // 3 秒后仍可访问：视为关闭失败，向前端返回结构化错误码而非静默成功。
    let error_code = probe.running.then(|| "OLLAMA_STOP_FAILED".to_owned());
    let snapshot = OllamaStatusSnapshot::from_probe(&probe, false, error_code);
    let _ = app.emit("ollama:state", &snapshot);
    snapshot
}

/// 不做进程内 panic（所有失败都转为结构化状态）；防重复启动由 `ensure_running`
/// 的端口探测保证，绝不重复 spawn。
fn start_in_background(app: AppHandle) {
    std::thread::spawn(move || {
        let _ = app.emit(
            "ollama:state",
            serde_json::json!({ "status": "installed_not_running", "starting": true }),
        );
        match ensure_running(OLLAMA_START_TIMEOUT) {
            Ok(ready) if ready.running => {
                let version = ready.version.map(|v| v.version).unwrap_or_default();
                let _ = app.emit(
                    "ollama:state",
                    serde_json::json!({ "status": "ready", "version": version }),
                );
                crate::runtime_log::event(
                    "info",
                    "ollama",
                    "ollama.started",
                    None,
                    &serde_json::json!({}),
                );
            }
            Ok(_) => {
                let _ = app.emit(
                    "ollama:state",
                    serde_json::json!({ "status": "installed_not_running", "starting": false }),
                );
                crate::runtime_log::event(
                    "warning",
                    "ollama",
                    "ollama.start_timeout",
                    None,
                    &serde_json::json!({}),
                );
            }
            Err(error) => {
                let _ = app.emit(
                    "ollama:state",
                    serde_json::json!({ "status": "installed_not_running", "starting": false, "error_code": error.code }),
                );
                crate::runtime_log::event(
                    "warning",
                    "ollama",
                    "ollama.start_failed",
                    None,
                    &serde_json::json!({ "error_code": error.code }),
                );
            }
        }
    });
}
