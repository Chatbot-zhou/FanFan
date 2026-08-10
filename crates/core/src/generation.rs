use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::AppError;

#[derive(Debug)]
pub struct LocalGenerationRuntime {
    executable: PathBuf,
    process: Option<GenerationProcess>,
}

#[derive(Debug)]
struct GenerationProcess {
    child: Child,
    port: u16,
    token: String,
    model_path: String,
    mmproj_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GenerationActivation {
    pub backend: String,
    pub model_path: String,
    pub context_size: u32,
    pub self_test: String,
    pub multimodal: bool,
}

impl LocalGenerationRuntime {
    pub fn new(executable: PathBuf) -> Self {
        Self {
            executable,
            process: None,
        }
    }

    pub fn executable_available(&self) -> bool {
        self.executable.is_file()
    }

    pub fn is_active(&mut self) -> bool {
        self.process
            .as_mut()
            .is_some_and(|process| process.child.try_wait().ok().flatten().is_none())
    }

    pub fn activate(
        &mut self,
        model_path: &str,
        context_size: u32,
        threads: u32,
    ) -> Result<GenerationActivation, AppError> {
        self.activate_internal(model_path, None, context_size, threads)
    }

    pub fn activate_multimodal(
        &mut self,
        model_path: &str,
        mmproj_path: &str,
        context_size: u32,
        threads: u32,
    ) -> Result<GenerationActivation, AppError> {
        let projector = Path::new(mmproj_path);
        if !projector.is_file()
            || projector
                .extension()
                .and_then(|value| value.to_str())
                .map(str::to_ascii_lowercase)
                .as_deref()
                != Some("gguf")
        {
            return Err(AppError::new(
                "VISION_PROJECTOR_INVALID",
                "图片理解模型需要可读取且与主模型匹配的mmproj GGUF文件",
                false,
            ));
        }
        self.activate_internal(model_path, Some(mmproj_path), context_size, threads)
    }

    fn activate_internal(
        &mut self,
        model_path: &str,
        mmproj_path: Option<&str>,
        context_size: u32,
        threads: u32,
    ) -> Result<GenerationActivation, AppError> {
        if !self.executable.is_file() {
            return Err(AppError::new(
                "GENERATION_RUNTIME_UNAVAILABLE",
                "llama.cpp本地运行组件缺失，请修复拾忆安装",
                false,
            ));
        }
        let model = Path::new(model_path);
        if !model.is_file()
            || model
                .extension()
                .and_then(|value| value.to_str())
                .map(str::to_ascii_lowercase)
                .as_deref()
                != Some("gguf")
        {
            return Err(AppError::new(
                "GENERATION_MODEL_INVALID",
                "生成模型必须是可读取的GGUF文件",
                false,
            ));
        }
        if !(1024..=32768).contains(&context_size) || !(1..=64).contains(&threads) {
            return Err(AppError::new(
                "GENERATION_RUNTIME_CONFIG_INVALID",
                "上下文或CPU线程配置超出安全范围",
                false,
            ));
        }
        self.stop();
        let port = reserve_local_port()?;
        let token = format!("remin-{}", uuid::Uuid::now_v7());
        let mut command = Command::new(&self.executable);
        command
            .args([
                "--model",
                model_path,
                "--host",
                "127.0.0.1",
                "--port",
                &port.to_string(),
                "--api-key",
                &token,
                "--ctx-size",
                &context_size.to_string(),
                "--threads",
                &threads.to_string(),
                "--n-gpu-layers",
                "0",
                "--jinja",
                "--sleep-idle-seconds",
                "300",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(mmproj_path) = mmproj_path {
            command.args(["--mmproj", mmproj_path, "--no-mmproj-offload"]);
        }
        hide_child_console(&mut command);
        let child = command.spawn().map_err(|error| {
            let mut err = AppError::new("GENERATION_RUNTIME_START_FAILED", "本地模型服务启动失败，请重启拾忆后重试", true);
            err.details = Some(Box::new(serde_json::json!({"technical": error.to_string()})));
            err
        })?;
        self.process = Some(GenerationProcess {
            child,
            port,
            token,
            model_path: model_path.to_owned(),
            mmproj_path: mmproj_path.map(str::to_owned),
        });
        let started = Instant::now();
        loop {
            if started.elapsed() > Duration::from_secs(180) {
                self.stop();
                return Err(AppError::new(
                    "GENERATION_RUNTIME_TIMEOUT",
                    "生成模型在3分钟内未能完成加载",
                    true,
                ));
            }
            if let Some(process) = self.process.as_mut()
                && let Some(status) = process.child.try_wait().map_err(|error| {
                    AppError::new("GENERATION_RUNTIME_CHECK_FAILED", "本地模型服务状态检查失败", true)
                })?
            {
                self.stop();
                return Err(AppError::new(
                    "GENERATION_RUNTIME_EXITED",
                    format!("llama.cpp启动后退出：{status}"),
                    true,
                ));
            }
            match http_request(port, "GET", "/health", None, None) {
                Ok((200, _)) => break,
                Ok((503, _)) | Err(_) => thread::sleep(Duration::from_millis(500)),
                Ok((status, body)) => {
                    self.stop();
                    return Err(AppError::new(
                        "GENERATION_RUNTIME_HEALTH_FAILED",
                        format!("健康检查返回{status}：{body}"),
                        true,
                    ));
                }
            }
        }
        let self_test = if mmproj_path.is_some() {
            self.complete_multimodal_data_url(
                "你是本地多模态运行状态检查器，不进行推理说明。",
                "只回复：就绪",
                "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=",
                64,
                None,
            )?
        } else {
            self.complete(
                "你是本地运行状态检查器，不进行推理说明。",
                "只回复：就绪",
                64,
            )?
        };
        if self_test.trim().is_empty() {
            self.stop();
            return Err(AppError::new(
                "GENERATION_SELF_TEST_FAILED",
                "生成模型自检没有返回文本",
                true,
            ));
        }
        Ok(GenerationActivation {
            backend: "cpu".into(),
            model_path: model_path.into(),
            context_size,
            self_test,
            multimodal: mmproj_path.is_some(),
        })
    }

    pub fn complete(
        &mut self,
        system: &str,
        user: &str,
        max_tokens: u32,
    ) -> Result<String, AppError> {
        self.complete_internal(system, user, max_tokens, None)
    }

    pub fn complete_cancellable(
        &mut self,
        system: &str,
        user: &str,
        max_tokens: u32,
        cancelled: &AtomicBool,
    ) -> Result<String, AppError> {
        self.complete_internal(system, user, max_tokens, Some(cancelled))
    }

    fn complete_internal(
        &mut self,
        system: &str,
        user: &str,
        max_tokens: u32,
        cancelled: Option<&AtomicBool>,
    ) -> Result<String, AppError> {
        let process = self.process.as_mut().ok_or_else(|| {
            AppError::new("GENERATION_RUNTIME_INACTIVE", "生成模型尚未启动", true)
        })?;
        if process
            .child
            .try_wait()
            .map_err(|error| {
                AppError::new("GENERATION_RUNTIME_CHECK_FAILED", "本地模型服务状态检查失败", true)
            })?
            .is_some()
        {
            self.process = None;
            return Err(AppError::new(
                "GENERATION_RUNTIME_EXITED",
                "生成模型运行进程已经退出",
                true,
            ));
        }
        let payload = json!({
            "model": "remin-local",
            "stream": false,
            "temperature": 0.1,
            "max_tokens": max_tokens.clamp(1, 2048),
            "chat_template_kwargs": {"enable_thinking": false},
            "messages": [
                {"role":"system","content":system},
                {"role":"user","content":user}
            ]
        });
        let body = serde_json::to_vec(&payload).map_err(|error| {
            AppError::new("GENERATION_REQUEST_INVALID", "本地模型请求构造失败", false)
        })?;
        let (status, response) = http_request_internal(
            process.port,
            "POST",
            "/v1/chat/completions",
            Some(&process.token),
            Some(&body),
            cancelled,
        )?;
        if status != 200 {
            return Err(AppError::new(
                "GENERATION_REQUEST_FAILED",
                format!("本地模型返回{status}：{response}"),
                status >= 500,
            ));
        }
        let value: serde_json::Value = serde_json::from_str(&response).map_err(|error| {
            AppError::new("GENERATION_RESPONSE_INVALID", "本地模型响应格式异常，请稍后重试", false)
        })?;
        value
            .pointer("/choices/0/message/content")
            .and_then(|value| value.as_str())
            .map(str::to_owned)
            .ok_or_else(|| {
                AppError::new(
                    "GENERATION_RESPONSE_INVALID",
                    "本地模型响应缺少回答文本",
                    false,
                )
            })
    }

    pub fn describe_image_cancellable(
        &mut self,
        system: &str,
        prompt: &str,
        image_path: &Path,
        mime_type: &str,
        max_tokens: u32,
        cancelled: &AtomicBool,
    ) -> Result<String, AppError> {
        if !matches!(
            mime_type,
            "image/jpeg" | "image/png" | "image/webp" | "image/bmp"
        ) {
            return Err(AppError::new(
                "VISION_IMAGE_FORMAT_UNSUPPORTED",
                "当前图片理解运行时只接受JPEG、PNG、WebP或BMP缓存",
                false,
            ));
        }
        let metadata = fs::metadata(image_path)
            .map_err(|error| AppError::new("VISION_IMAGE_UNAVAILABLE", "图片缓存不可用，请稍后重试", true))?;
        if !metadata.is_file() || metadata.len() == 0 || metadata.len() > 32 * 1024 * 1024 {
            return Err(AppError::new(
                "VISION_IMAGE_SIZE_UNSUPPORTED",
                "图片理解缓存必须是1字节到32MB的普通文件",
                false,
            ));
        }
        let bytes = fs::read(image_path)
            .map_err(|error| AppError::new("VISION_IMAGE_UNAVAILABLE", "图片缓存不可用，请稍后重试", true))?;
        let data_url = format!("data:{mime_type};base64,{}", STANDARD.encode(bytes));
        self.complete_multimodal_data_url(system, prompt, &data_url, max_tokens, Some(cancelled))
    }

    fn complete_multimodal_data_url(
        &mut self,
        system: &str,
        prompt: &str,
        data_url: &str,
        max_tokens: u32,
        cancelled: Option<&AtomicBool>,
    ) -> Result<String, AppError> {
        let process = self.process.as_mut().ok_or_else(|| {
            AppError::new("VISION_RUNTIME_INACTIVE", "图片理解模型尚未启动", true)
        })?;
        if process.mmproj_path.is_none() {
            return Err(AppError::new(
                "VISION_RUNTIME_INACTIVE",
                "当前本地模型进程没有加载多模态投影组件",
                true,
            ));
        }
        if process
            .child
            .try_wait()
            .map_err(|error| AppError::new("VISION_RUNTIME_CHECK_FAILED", "图片理解模型状态检查失败", true))?
            .is_some()
        {
            self.process = None;
            return Err(AppError::new(
                "VISION_RUNTIME_EXITED",
                "图片理解模型运行进程已经退出",
                true,
            ));
        }
        let payload = json!({
            "model": "remin-local-vision",
            "stream": false,
            "temperature": 0.1,
            "max_tokens": max_tokens.clamp(1, 2048),
            "chat_template_kwargs": {"enable_thinking": false},
            "messages": [
                {"role":"system","content":system},
                {"role":"user","content":[
                    {"type":"text","text":prompt},
                    {"type":"image_url","image_url":{"url":data_url}}
                ]}
            ]
        });
        let body = serde_json::to_vec(&payload)
            .map_err(|error| AppError::new("VISION_REQUEST_INVALID", "图片理解请求构造失败", false))?;
        let (status, response) = http_request_internal(
            process.port,
            "POST",
            "/v1/chat/completions",
            Some(&process.token),
            Some(&body),
            cancelled,
        )?;
        if status != 200 {
            return Err(AppError::new(
                "VISION_REQUEST_FAILED",
                format!("本地多模态模型返回{status}：{response}"),
                status >= 500,
            ));
        }
        let value: serde_json::Value = serde_json::from_str(&response)
            .map_err(|error| AppError::new("VISION_RESPONSE_INVALID", "图片理解响应格式异常", false))?;
        value
            .pointer("/choices/0/message/content")
            .and_then(|value| value.as_str())
            .map(str::to_owned)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                AppError::new(
                    "VISION_RESPONSE_INVALID",
                    "本地多模态模型响应缺少图片说明文本",
                    false,
                )
            })
    }

    pub fn active_model_path(&self) -> Option<&str> {
        self.process
            .as_ref()
            .map(|process| process.model_path.as_str())
    }

    pub fn active_mmproj_path(&self) -> Option<&str> {
        self.process
            .as_ref()
            .and_then(|process| process.mmproj_path.as_deref())
    }

    pub fn stop(&mut self) {
        if let Some(mut process) = self.process.take() {
            let _ = process.child.kill();
            let _ = process.child.wait();
        }
    }
}

impl Drop for LocalGenerationRuntime {
    fn drop(&mut self) {
        self.stop();
    }
}

fn reserve_local_port() -> Result<u16, AppError> {
    TcpListener::bind(("127.0.0.1", 0))
        .and_then(|listener| listener.local_addr())
        .map(|address| address.port())
        .map_err(|error| AppError::new("GENERATION_PORT_UNAVAILABLE", "本地模型通信端口异常，请重启拾忆", true))
}

fn http_request(
    port: u16,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&[u8]>,
) -> Result<(u16, String), AppError> {
    http_request_internal(port, method, path, token, body, None)
}

fn http_request_internal(
    port: u16,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&[u8]>,
    cancelled: Option<&AtomicBool>,
) -> Result<(u16, String), AppError> {
    const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
    let mut stream = TcpStream::connect_timeout(
        &format!("127.0.0.1:{port}")
            .parse()
            .expect("loopback socket"),
        Duration::from_secs(3),
    )
    .map_err(|error| AppError::new("GENERATION_RUNTIME_UNREACHABLE", "本地模型服务无响应，请重启拾忆后重试", true))?;
    stream
        .set_read_timeout(Some(if cancelled.is_some() {
            Duration::from_millis(250)
        } else {
            Duration::from_secs(180)
        }))
        .map_err(|error| AppError::new("GENERATION_RUNTIME_IO_FAILED", "本地模型通信失败，请重启拾忆后重试", true))?;
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .map_err(|error| AppError::new("GENERATION_RUNTIME_IO_FAILED", "本地模型通信失败，请重启拾忆后重试", true))?;
    let payload = body.unwrap_or_default();
    let authorization = token
        .map(|value| format!("Authorization: Bearer {value}\r\n"))
        .unwrap_or_default();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\n{authorization}Content-Length: {}\r\nConnection: close\r\n\r\n",
        payload.len()
    );
    stream
        .write_all(request.as_bytes())
        .and_then(|_| stream.write_all(payload))
        .and_then(|_| stream.flush())
        .map_err(|error| AppError::new("GENERATION_RUNTIME_IO_FAILED", "本地模型通信失败，请重启拾忆后重试", true))?;
    let mut response = Vec::new();
    let started = Instant::now();
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        if cancelled.is_some_and(|flag| flag.load(Ordering::Acquire)) {
            return Err(AppError::new("OPERATION_CANCELLED", "问答已取消", false));
        }
        if started.elapsed() > Duration::from_secs(180) {
            return Err(AppError::new(
                "GENERATION_RUNTIME_IO_FAILED",
                "本地模型响应超过180秒时限",
                true,
            ));
        }
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                response.extend_from_slice(&buffer[..read]);
                if response.len() > MAX_RESPONSE_BYTES {
                    return Err(AppError::new(
                        "GENERATION_RESPONSE_TOO_LARGE",
                        "本地模型响应超过8 MB安全上限",
                        false,
                    ));
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(error) => {
                return Err(AppError::new(
                    "GENERATION_RUNTIME_IO_FAILED",
                    error.to_string(),
                    true,
                ));
            }
        }
    }
    parse_http_response(&response)
}

fn parse_http_response(response: &[u8]) -> Result<(u16, String), AppError> {
    let separator = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| {
            AppError::new(
                "GENERATION_RESPONSE_INVALID",
                "本地模型HTTP响应不完整",
                false,
            )
        })?;
    let headers = std::str::from_utf8(&response[..separator])
        .map_err(|error| AppError::new("GENERATION_RESPONSE_INVALID", "本地模型响应格式异常，请稍后重试", false))?;
    let raw_body = &response[separator + 4..];
    let status = headers
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u16>().ok())
        .ok_or_else(|| {
            AppError::new("GENERATION_RESPONSE_INVALID", "本地模型HTTP状态无效", false)
        })?;
    let chunked = headers.lines().any(|line| {
        line.split_once(':').is_some_and(|(name, value)| {
            name.eq_ignore_ascii_case("transfer-encoding")
                && value
                    .split(',')
                    .any(|encoding| encoding.trim().eq_ignore_ascii_case("chunked"))
        })
    });
    let body = if chunked {
        decode_chunked_body(raw_body)?
    } else {
        if let Some(content_length) = headers.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        }) && raw_body.len() < content_length
        {
            return Err(AppError::new(
                "GENERATION_RESPONSE_INVALID",
                "本地模型HTTP响应正文未接收完整",
                true,
            ));
        }
        raw_body.to_vec()
    };
    String::from_utf8(body)
        .map(|body| (status, body))
        .map_err(|error| AppError::new("GENERATION_RESPONSE_INVALID", "本地模型响应格式异常，请稍后重试", false))
}

fn decode_chunked_body(mut input: &[u8]) -> Result<Vec<u8>, AppError> {
    let mut output = Vec::new();
    loop {
        let line_end = input
            .windows(2)
            .position(|window| window == b"\r\n")
            .ok_or_else(|| {
                AppError::new(
                    "GENERATION_RESPONSE_INVALID",
                    "本地模型分块响应缺少长度行",
                    false,
                )
            })?;
        let length_text = std::str::from_utf8(&input[..line_end]).map_err(|error| {
            AppError::new("GENERATION_RESPONSE_INVALID", "本地模型响应格式异常，请稍后重试", false)
        })?;
        let length = usize::from_str_radix(length_text.split(';').next().unwrap_or("").trim(), 16)
            .map_err(|error| {
                AppError::new("GENERATION_RESPONSE_INVALID", "本地模型响应格式异常，请稍后重试", false)
            })?;
        input = &input[line_end + 2..];
        if length == 0 {
            return Ok(output);
        }
        if input.len() < length + 2 || &input[length..length + 2] != b"\r\n" {
            return Err(AppError::new(
                "GENERATION_RESPONSE_INVALID",
                "本地模型分块响应正文未接收完整",
                true,
            ));
        }
        output.extend_from_slice(&input[..length]);
        input = &input[length + 2..];
    }
}

#[cfg(windows)]
fn hide_child_console(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    command.creation_flags(0x0800_0000);
}

#[cfg(not(windows))]
fn hide_child_console(_command: &mut Command) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_runtime_fails_before_model_is_touched() {
        let mut runtime = LocalGenerationRuntime::new(PathBuf::from("missing-llama-server.exe"));
        assert_eq!(
            runtime.activate("missing.gguf", 4096, 2).unwrap_err().code,
            "GENERATION_RUNTIME_UNAVAILABLE"
        );
    }

    #[test]
    fn multimodal_activation_requires_projector_before_starting_runtime() {
        let mut runtime = LocalGenerationRuntime::new(PathBuf::from("missing-llama-server.exe"));
        assert_eq!(
            runtime
                .activate_multimodal("missing.gguf", "missing-mmproj.gguf", 4096, 2)
                .unwrap_err()
                .code,
            "VISION_PROJECTOR_INVALID"
        );
    }

    #[test]
    fn parses_content_length_and_chunked_local_responses() {
        let plain = b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\n{\"ok\":true}";
        assert_eq!(
            parse_http_response(plain).unwrap(),
            (200, "{\"ok\":true}".into())
        );
        let chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\n{\"ok\"\r\n6\r\n:true}\r\n0\r\n\r\n";
        assert_eq!(
            parse_http_response(chunked).unwrap(),
            (200, "{\"ok\":true}".into())
        );
    }

    #[test]
    fn rejects_truncated_local_response() {
        let response = b"HTTP/1.1 200 OK\r\nContent-Length: 20\r\n\r\n{}";
        assert_eq!(
            parse_http_response(response).unwrap_err().code,
            "GENERATION_RESPONSE_INVALID"
        );
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "requires pinned llama.cpp runtime and Qwen3 GGUF"]
    fn real_llama_cpp_runtime_generates_locally() {
        let executable = std::env::var("REMIN_LLAMA_SERVER")
            .map(PathBuf::from)
            .expect("REMIN_LLAMA_SERVER is required");
        let model = std::env::var("REMIN_TEST_GGUF").expect("REMIN_TEST_GGUF is required");
        let mut runtime = LocalGenerationRuntime::new(executable);
        let activation = runtime
            .activate(&model, 1024, 2)
            .expect("activate pinned local runtime");
        assert_eq!(activation.backend, "cpu");
        assert!(!activation.self_test.trim().is_empty());
        let answer = runtime
            .complete(
                "你是离线验收程序，只执行格式要求。",
                "只回复 REMIN-LOCAL-OK，不要解释。",
                32,
            )
            .expect("generate local answer");
        assert!(!answer.trim().is_empty());
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "requires pinned llama.cpp runtime, multimodal GGUF and matching mmproj"]
    fn real_llama_cpp_runtime_understands_an_image_locally() {
        let executable = std::env::var("REMIN_LLAMA_SERVER")
            .map(PathBuf::from)
            .expect("REMIN_LLAMA_SERVER is required");
        let model =
            std::env::var("REMIN_TEST_VISION_GGUF").expect("REMIN_TEST_VISION_GGUF is required");
        let projector = std::env::var("REMIN_TEST_VISION_MMPROJ")
            .expect("REMIN_TEST_VISION_MMPROJ is required");
        let image = std::env::var("REMIN_TEST_IMAGE").expect("REMIN_TEST_IMAGE is required");
        let mut runtime = LocalGenerationRuntime::new(executable);
        let activation = runtime
            .activate_multimodal(&model, &projector, 2048, 2)
            .expect("activate pinned multimodal runtime");
        assert!(activation.multimodal);
        let answer = runtime
            .describe_image_cancellable(
                "只描述可见内容。",
                "用一句中文描述图片。",
                Path::new(&image),
                "image/png",
                128,
                &AtomicBool::new(false),
            )
            .expect("understand local image");
        assert!(!answer.trim().is_empty());
    }
}
