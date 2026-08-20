use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::AppError;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeBackendKind {
    LlamaCpp,
    OnnxRuntime,
    SherpaOnnx,
    PaddleOcr,
    Parser,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeTaskKind {
    Cancel,
    Preview,
    SpeechRecognition,
    Ask,
    DeepImageAnalysis,
    Search,
    Embedding,
    Rerank,
    IncrementalIndex,
    Ocr,
    ImageUnderstanding,
    CollectionAnalysis,
    Maintenance,
    Parse,
}

impl RuntimeTaskKind {
    pub fn default_priority(self) -> u8 {
        match self {
            Self::Cancel | Self::Preview => 0,
            Self::SpeechRecognition | Self::Ask | Self::DeepImageAnalysis => 1,
            Self::Search | Self::Embedding | Self::Rerank => 2,
            Self::IncrementalIndex => 3,
            Self::Ocr
            | Self::ImageUnderstanding
            | Self::CollectionAnalysis
            | Self::Maintenance
            | Self::Parse => 4,
        }
    }

    pub fn is_heavy(self) -> bool {
        matches!(
            self,
            Self::Ask | Self::DeepImageAnalysis | Self::ImageUnderstanding | Self::IncrementalIndex
        )
    }

    pub fn is_background(self) -> bool {
        matches!(
            self,
            Self::IncrementalIndex
                | Self::Ocr
                | Self::ImageUnderstanding
                | Self::CollectionAnalysis
                | Self::Maintenance
                | Self::Parse
        )
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeTaskState {
    Queued,
    Running,
    Completed,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RuntimeResourceBudget {
    pub physical_core_count: u32,
    pub foreground_cpu_threads: u32,
    pub background_cpu_threads: u32,
    pub total_memory_bytes: u64,
    pub reserved_memory_bytes: u64,
    pub total_gpu_memory_bytes: Option<u64>,
    pub reserved_gpu_memory_bytes: Option<u64>,
}

impl RuntimeResourceBudget {
    pub fn conservative(physical_core_count: u32, total_memory_bytes: u64) -> Self {
        let physical_core_count = physical_core_count.max(1);
        let foreground_cpu_threads = physical_core_count.div_ceil(2).clamp(1, 4);
        let background_cpu_threads = physical_core_count.clamp(1, 2);
        Self {
            physical_core_count,
            foreground_cpu_threads,
            background_cpu_threads,
            total_memory_bytes,
            reserved_memory_bytes: (total_memory_bytes / 5).max(2 * 1024 * 1024 * 1024),
            total_gpu_memory_bytes: None,
            reserved_gpu_memory_bytes: None,
        }
    }

    pub fn with_gpu_memory(mut self, total_gpu_memory_bytes: u64) -> Self {
        self.total_gpu_memory_bytes = Some(total_gpu_memory_bytes);
        self.reserved_gpu_memory_bytes =
            Some((total_gpu_memory_bytes.saturating_mul(15) / 100).max(512 * 1024 * 1024));
        self
    }
}

#[derive(Debug, Clone)]
pub struct RuntimeTaskRequest {
    pub kind: RuntimeTaskKind,
    pub backend: RuntimeBackendKind,
    pub model_id: Option<String>,
    pub cpu_threads: u32,
    pub memory_bytes: u64,
    pub gpu_memory_bytes: u64,
    pub timeout: Duration,
    pub idempotency_key: Option<String>,
}

impl RuntimeTaskRequest {
    pub fn interactive(kind: RuntimeTaskKind, backend: RuntimeBackendKind) -> Self {
        Self {
            kind,
            backend,
            model_id: None,
            cpu_threads: 1,
            memory_bytes: 0,
            gpu_memory_bytes: 0,
            timeout: Duration::from_secs(30),
            idempotency_key: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RuntimeOperationHandle {
    pub operation_id: Uuid,
    pub kind: RuntimeTaskKind,
    pub backend: RuntimeBackendKind,
    pub state: RuntimeTaskState,
    pub priority: u8,
    pub model_id: Option<String>,
    pub idempotency_key: Option<String>,
    pub queued_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub wait_ms: Option<u64>,
    pub error_code: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RuntimeInstanceState {
    pub instance_id: Uuid,
    pub backend: RuntimeBackendKind,
    pub model_id: Option<String>,
    pub device: String,
    pub status: String,
    pub memory_bytes: u64,
    pub gpu_memory_bytes: u64,
    pub idle_timeout_seconds: u64,
    pub loaded_at: DateTime<Utc>,
    pub last_used_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AiRuntimeSnapshot {
    pub budget: RuntimeResourceBudget,
    pub tasks: Vec<RuntimeOperationHandle>,
    pub instances: Vec<RuntimeInstanceState>,
    pub queued_count: usize,
    pub running_count: usize,
    pub active_heavy_task: Option<Uuid>,
    pub pressure_reason: Option<String>,
    pub checked_at: DateTime<Utc>,
}

#[derive(Debug)]
struct RuntimeState {
    budget: RuntimeResourceBudget,
    tasks: BTreeMap<Uuid, RuntimeOperationHandle>,
    requests: HashMap<Uuid, RuntimeTaskRequest>,
    instances: HashMap<Uuid, RuntimeInstanceState>,
    pressure_reason: Option<String>,
}

#[derive(Debug)]
struct RuntimeManagerInner {
    state: Mutex<RuntimeState>,
    changed: Condvar,
}

#[derive(Debug, Clone)]
pub struct RuntimeManager {
    inner: Arc<RuntimeManagerInner>,
}

impl RuntimeManager {
    pub fn new(budget: RuntimeResourceBudget) -> Self {
        Self {
            inner: Arc::new(RuntimeManagerInner {
                state: Mutex::new(RuntimeState {
                    budget,
                    tasks: BTreeMap::new(),
                    requests: HashMap::new(),
                    instances: HashMap::new(),
                    pressure_reason: None,
                }),
                changed: Condvar::new(),
            }),
        }
    }

    pub fn acquire(&self, request: RuntimeTaskRequest) -> Result<RuntimeLease, AppError> {
        let operation_id = Uuid::now_v7();
        let queued_at = Utc::now();
        let started_wait = Instant::now();
        let deadline = started_wait + request.timeout;
        let mut state = self.inner.state.lock().map_err(|_| runtime_lock_error())?;
        self.validate_request(&state.budget, &request)?;
        state.tasks.insert(
            operation_id,
            RuntimeOperationHandle {
                operation_id,
                kind: request.kind,
                backend: request.backend,
                state: RuntimeTaskState::Queued,
                priority: request.kind.default_priority(),
                model_id: request.model_id.clone(),
                idempotency_key: request.idempotency_key.clone(),
                queued_at,
                started_at: None,
                finished_at: None,
                wait_ms: None,
                error_code: None,
            },
        );
        state.requests.insert(operation_id, request);

        loop {
            if self.can_start(&state, operation_id) {
                let started_at = Utc::now();
                let wait_ms = started_wait.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
                if let Some(task) = state.tasks.get_mut(&operation_id) {
                    task.state = RuntimeTaskState::Running;
                    task.started_at = Some(started_at);
                    task.wait_ms = Some(wait_ms);
                }
                return Ok(RuntimeLease {
                    manager: self.clone(),
                    operation_id,
                    released: false,
                });
            }

            let now = Instant::now();
            if now >= deadline {
                mark_finished(
                    &mut state,
                    operation_id,
                    RuntimeTaskState::Failed,
                    Some("RUNTIME_LEASE_TIMEOUT".into()),
                );
                self.inner.changed.notify_all();
                return Err(AppError::new(
                    "RUNTIME_LEASE_TIMEOUT",
                    "本地 AI 资源仍在忙，请稍后重试",
                    true,
                ));
            }
            let remaining = deadline.saturating_duration_since(now);
            let (next_state, _) = self
                .inner
                .changed
                .wait_timeout(state, remaining)
                .map_err(|_| runtime_lock_error())?;
            state = next_state;
            if state
                .tasks
                .get(&operation_id)
                .is_some_and(|task| task.state == RuntimeTaskState::Cancelled)
            {
                return Err(AppError::new("RUNTIME_TASK_CANCELLED", "任务已取消", false));
            }
        }
    }

    pub fn cancel(&self, operation_id: &Uuid) -> Result<bool, AppError> {
        let mut state = self.inner.state.lock().map_err(|_| runtime_lock_error())?;
        let Some(task) = state.tasks.get(operation_id) else {
            return Ok(false);
        };
        if matches!(
            task.state,
            RuntimeTaskState::Completed | RuntimeTaskState::Cancelled | RuntimeTaskState::Failed
        ) {
            return Ok(false);
        }
        mark_finished(
            &mut state,
            *operation_id,
            RuntimeTaskState::Cancelled,
            Some("RUNTIME_TASK_CANCELLED".into()),
        );
        self.inner.changed.notify_all();
        Ok(true)
    }

    pub fn cancel_all(&self) -> Result<usize, AppError> {
        let mut state = self.inner.state.lock().map_err(|_| runtime_lock_error())?;
        let active = state
            .tasks
            .values()
            .filter(|task| {
                matches!(
                    task.state,
                    RuntimeTaskState::Queued | RuntimeTaskState::Running
                )
            })
            .map(|task| task.operation_id)
            .collect::<Vec<_>>();
        for operation_id in &active {
            mark_finished(
                &mut state,
                *operation_id,
                RuntimeTaskState::Cancelled,
                Some("RUNTIME_SHUTDOWN".into()),
            );
        }
        self.inner.changed.notify_all();
        Ok(active.len())
    }

    pub fn set_pressure(&self, reason: Option<String>) -> Result<(), AppError> {
        let mut state = self.inner.state.lock().map_err(|_| runtime_lock_error())?;
        state.pressure_reason = reason;
        self.inner.changed.notify_all();
        Ok(())
    }

    pub fn snapshot(&self) -> Result<AiRuntimeSnapshot, AppError> {
        let state = self.inner.state.lock().map_err(|_| runtime_lock_error())?;
        let mut tasks = state.tasks.values().cloned().collect::<Vec<_>>();
        tasks.sort_by_key(|task| std::cmp::Reverse(task.queued_at));
        tasks.truncate(64);
        let instances = state.instances.values().cloned().collect::<Vec<_>>();
        let queued_count = state
            .tasks
            .values()
            .filter(|task| task.state == RuntimeTaskState::Queued)
            .count();
        let running = state
            .tasks
            .values()
            .filter(|task| task.state == RuntimeTaskState::Running)
            .collect::<Vec<_>>();
        Ok(AiRuntimeSnapshot {
            budget: state.budget.clone(),
            tasks,
            instances,
            queued_count,
            running_count: running.len(),
            active_heavy_task: running
                .iter()
                .find(|task| task.kind.is_heavy())
                .map(|task| task.operation_id),
            pressure_reason: state.pressure_reason.clone(),
            checked_at: Utc::now(),
        })
    }

    pub fn register_instance(&self, instance: RuntimeInstanceState) -> Result<(), AppError> {
        let mut state = self.inner.state.lock().map_err(|_| runtime_lock_error())?;
        state.instances.insert(instance.instance_id, instance);
        Ok(())
    }

    /// 同步 sidecar 进程的实时观测（内存/存活），已存在的实例只更新
    /// 观测字段并保持首次加载时间；进程死亡时用 unregister 清除。
    pub fn sync_instance(&self, instance: RuntimeInstanceState) -> Result<(), AppError> {
        let mut state = self.inner.state.lock().map_err(|_| runtime_lock_error())?;
        if let Some(existing) = state.instances.get_mut(&instance.instance_id) {
            existing.memory_bytes = instance.memory_bytes;
            existing.gpu_memory_bytes = instance.gpu_memory_bytes;
            existing.status = instance.status;
            existing.last_used_at = Utc::now();
            return Ok(());
        }
        state.instances.insert(instance.instance_id, instance);
        Ok(())
    }

    pub fn unregister_instance(&self, instance_id: &Uuid) -> Result<bool, AppError> {
        let mut state = self.inner.state.lock().map_err(|_| runtime_lock_error())?;
        Ok(state.instances.remove(instance_id).is_some())
    }

    pub fn touch_instance(&self, instance_id: &Uuid) -> Result<bool, AppError> {
        let mut state = self.inner.state.lock().map_err(|_| runtime_lock_error())?;
        let Some(instance) = state.instances.get_mut(instance_id) else {
            return Ok(false);
        };
        instance.last_used_at = Utc::now();
        Ok(true)
    }

    pub fn reap_idle_instances(&self, now: DateTime<Utc>) -> Result<Vec<Uuid>, AppError> {
        let mut state = self.inner.state.lock().map_err(|_| runtime_lock_error())?;
        let expired = state
            .instances
            .values()
            .filter(|instance| {
                now.signed_duration_since(instance.last_used_at)
                    .num_seconds()
                    >= instance.idle_timeout_seconds as i64
            })
            .map(|instance| instance.instance_id)
            .collect::<Vec<_>>();
        for instance_id in &expired {
            state.instances.remove(instance_id);
        }
        Ok(expired)
    }

    fn validate_request(
        &self,
        budget: &RuntimeResourceBudget,
        request: &RuntimeTaskRequest,
    ) -> Result<(), AppError> {
        let thread_limit = if request.kind.is_background() {
            budget.background_cpu_threads
        } else {
            budget.foreground_cpu_threads
        };
        if request.cpu_threads == 0 || request.cpu_threads > thread_limit {
            return Err(AppError::new(
                "RUNTIME_THREAD_BUDGET_EXCEEDED",
                format!(
                    "任务请求{}个线程，当前预算为{}个",
                    request.cpu_threads, thread_limit
                ),
                false,
            ));
        }
        if request
            .memory_bytes
            .saturating_add(budget.reserved_memory_bytes)
            > budget.total_memory_bytes
        {
            return Err(AppError::new(
                "RUNTIME_MEMORY_BUDGET_EXCEEDED",
                "当前可用内存不足以安全启动模型",
                true,
            ));
        }
        if let (Some(total), Some(reserved)) = (
            budget.total_gpu_memory_bytes,
            budget.reserved_gpu_memory_bytes,
        ) && request.gpu_memory_bytes.saturating_add(reserved) > total
        {
            return Err(AppError::new(
                "RUNTIME_GPU_MEMORY_BUDGET_EXCEEDED",
                "当前可用显存不足以安全启动模型",
                true,
            ));
        }
        Ok(())
    }

    fn can_start(&self, state: &RuntimeState, operation_id: Uuid) -> bool {
        let Some(task) = state.tasks.get(&operation_id) else {
            return false;
        };
        let Some(request) = state.requests.get(&operation_id) else {
            return false;
        };
        if task.state != RuntimeTaskState::Queued {
            return false;
        }
        if request.kind.is_background() && state.pressure_reason.is_some() {
            return false;
        }
        let first_waiting = state
            .tasks
            .values()
            .filter(|candidate| candidate.state == RuntimeTaskState::Queued)
            .min_by_key(|candidate| {
                (
                    candidate.priority,
                    candidate.queued_at,
                    candidate.operation_id,
                )
            });
        if first_waiting.is_some_and(|candidate| candidate.operation_id != operation_id) {
            return false;
        }
        let running = state
            .tasks
            .values()
            .filter(|candidate| candidate.state == RuntimeTaskState::Running)
            .collect::<Vec<_>>();
        if request.kind.is_background() && running.iter().any(|candidate| candidate.priority <= 2) {
            return false;
        }
        if request.kind.is_heavy() && running.iter().any(|candidate| candidate.kind.is_heavy()) {
            return false;
        }
        if running
            .iter()
            .any(|candidate| candidate.backend == request.backend)
        {
            return false;
        }
        let requested = state
            .requests
            .get(&operation_id)
            .expect("queued task has request");
        let running_memory = running
            .iter()
            .filter_map(|task| state.requests.get(&task.operation_id))
            .map(|task| task.memory_bytes)
            .sum::<u64>();
        running_memory
            .saturating_add(requested.memory_bytes)
            .saturating_add(state.budget.reserved_memory_bytes)
            <= state.budget.total_memory_bytes
    }

    fn finish(&self, operation_id: Uuid, status: RuntimeTaskState, error_code: Option<String>) {
        if let Ok(mut state) = self.inner.state.lock() {
            mark_finished(&mut state, operation_id, status, error_code);
            self.inner.changed.notify_all();
        }
    }
}

#[derive(Debug)]
pub struct RuntimeLease {
    manager: RuntimeManager,
    operation_id: Uuid,
    released: bool,
}

impl RuntimeLease {
    pub fn operation_id(&self) -> Uuid {
        self.operation_id
    }

    pub fn complete(mut self) {
        self.manager
            .finish(self.operation_id, RuntimeTaskState::Completed, None);
        self.released = true;
    }

    pub fn fail(mut self, error_code: impl Into<String>) {
        self.manager.finish(
            self.operation_id,
            RuntimeTaskState::Failed,
            Some(error_code.into()),
        );
        self.released = true;
    }
}

impl Drop for RuntimeLease {
    fn drop(&mut self) {
        if !self.released {
            self.manager.finish(
                self.operation_id,
                RuntimeTaskState::Failed,
                Some("RUNTIME_LEASE_DROPPED".into()),
            );
        }
    }
}

fn mark_finished(
    state: &mut RuntimeState,
    operation_id: Uuid,
    status: RuntimeTaskState,
    error_code: Option<String>,
) {
    if let Some(task) = state.tasks.get_mut(&operation_id) {
        task.state = status;
        task.finished_at = Some(Utc::now());
        task.error_code = error_code;
    }
}

fn runtime_lock_error() -> AppError {
    AppError::new(
        "RUNTIME_MANAGER_UNAVAILABLE",
        "本地 AI 运行时状态暂时不可用",
        true,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::mpsc, thread};

    fn manager() -> RuntimeManager {
        RuntimeManager::new(RuntimeResourceBudget::conservative(
            8,
            16 * 1024 * 1024 * 1024,
        ))
    }

    #[test]
    fn conservative_budget_preserves_system_resources() {
        let budget = RuntimeResourceBudget::conservative(16, 8 * 1024 * 1024 * 1024)
            .with_gpu_memory(4 * 1024 * 1024 * 1024);
        assert_eq!(budget.foreground_cpu_threads, 4);
        assert_eq!(budget.background_cpu_threads, 2);
        assert_eq!(budget.reserved_memory_bytes, 2 * 1024 * 1024 * 1024);
        assert_eq!(budget.reserved_gpu_memory_bytes, Some(644_245_094));
    }

    #[test]
    fn heavy_model_tasks_are_serialized() {
        let manager = manager();
        let first = manager
            .acquire(RuntimeTaskRequest::interactive(
                RuntimeTaskKind::Ask,
                RuntimeBackendKind::LlamaCpp,
            ))
            .expect("first lease");
        let (sender, receiver) = mpsc::channel();
        let second_manager = manager.clone();
        thread::spawn(move || {
            let mut request = RuntimeTaskRequest::interactive(
                RuntimeTaskKind::DeepImageAnalysis,
                RuntimeBackendKind::LlamaCpp,
            );
            request.timeout = Duration::from_secs(1);
            let lease = second_manager.acquire(request).expect("second lease");
            sender.send(lease.operation_id()).expect("send id");
        });
        assert!(receiver.recv_timeout(Duration::from_millis(80)).is_err());
        first.complete();
        assert!(receiver.recv_timeout(Duration::from_secs(1)).is_ok());
    }

    #[test]
    fn pressure_pauses_new_background_work() {
        let manager = manager();
        manager
            .set_pressure(Some("系统内存压力较高".into()))
            .expect("set pressure");
        let mut request =
            RuntimeTaskRequest::interactive(RuntimeTaskKind::Ocr, RuntimeBackendKind::PaddleOcr);
        request.timeout = Duration::from_millis(30);
        let error = manager.acquire(request).expect_err("background must pause");
        assert_eq!(error.code, "RUNTIME_LEASE_TIMEOUT");
    }

    #[test]
    fn idle_instances_are_reaped() {
        let manager = manager();
        let instance_id = Uuid::now_v7();
        let old = Utc::now() - chrono::Duration::seconds(70);
        manager
            .register_instance(RuntimeInstanceState {
                instance_id,
                backend: RuntimeBackendKind::OnnxRuntime,
                model_id: Some("embedding".into()),
                device: "cpu".into(),
                status: "idle".into(),
                memory_bytes: 1,
                gpu_memory_bytes: 0,
                idle_timeout_seconds: 60,
                loaded_at: old,
                last_used_at: old,
            })
            .expect("register");
        assert_eq!(
            manager.reap_idle_instances(Utc::now()).unwrap(),
            vec![instance_id]
        );
    }

    #[test]
    fn sync_instance_updates_observations_but_keeps_loaded_at() {
        let manager = manager();
        let instance_id = Uuid::now_v7();
        let loaded_at = Utc::now() - chrono::Duration::minutes(5);
        let mut instance = RuntimeInstanceState {
            instance_id,
            backend: RuntimeBackendKind::OnnxRuntime,
            model_id: None,
            device: "cpu".into(),
            status: "running".into(),
            memory_bytes: 1,
            gpu_memory_bytes: 0,
            idle_timeout_seconds: 60,
            loaded_at,
            last_used_at: loaded_at,
        };
        manager
            .register_instance(instance.clone())
            .expect("register");
        instance.memory_bytes = 2_000_000_000;
        instance.status = "idle".into();
        manager.sync_instance(instance.clone()).expect("sync");
        let snapshot = manager.snapshot().expect("snapshot");
        let found = snapshot
            .instances
            .iter()
            .find(|entry| entry.instance_id == instance_id)
            .expect("instance exists");
        assert_eq!(found.memory_bytes, 2_000_000_000);
        assert_eq!(found.status, "idle");
        assert_eq!(found.loaded_at, loaded_at);
    }

    #[test]
    fn unregister_instance_removes_entry() {
        let manager = manager();
        let instance_id = Uuid::now_v7();
        manager
            .register_instance(RuntimeInstanceState {
                instance_id,
                backend: RuntimeBackendKind::SherpaOnnx,
                model_id: None,
                device: "cpu".into(),
                status: "running".into(),
                memory_bytes: 1,
                gpu_memory_bytes: 0,
                idle_timeout_seconds: 60,
                loaded_at: Utc::now(),
                last_used_at: Utc::now(),
            })
            .expect("register");
        assert!(manager.unregister_instance(&instance_id).unwrap());
        assert!(!manager.unregister_instance(&instance_id).unwrap());
        assert!(manager.snapshot().unwrap().instances.is_empty());
    }
}
