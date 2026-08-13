use std::sync::{Arc, Mutex};

use fanfan_core::AppError;
use serde::Serialize;
use serde_json::json;
use tauri::{AppHandle, Emitter, State};

#[derive(Debug, Clone, Serialize)]
pub struct StartupState {
    pub phase: &'static str,
    pub ready: bool,
    pub progress: f64,
    pub pending_files: u64,
    pub blocker: Option<AppError>,
    pub recovery_actions: Vec<&'static str>,
}

impl Default for StartupState {
    fn default() -> Self {
        Self {
            phase: "opening_catalog",
            ready: false,
            progress: 0.1,
            pending_files: 0,
            blocker: None,
            recovery_actions: Vec::new(),
        }
    }
}

#[derive(Clone, Default)]
pub struct StartupServiceState(pub Arc<Mutex<StartupState>>);

impl StartupServiceState {
    pub fn publish(&self, app: &AppHandle, next: StartupState) {
        crate::runtime_log::event(
            if next.blocker.is_some() {
                "error"
            } else {
                "info"
            },
            "startup",
            "startup.state_changed",
            None,
            &json!({
                "phase": next.phase,
                "ready": next.ready,
                "progress": next.progress,
                "pending_files": next.pending_files,
                "error_code": next.blocker.as_ref().map(|error| error.code.as_str()),
                "retryable": next.blocker.as_ref().map(|error| error.retryable),
                "recovery_actions": &next.recovery_actions,
            }),
        );
        if let Ok(mut current) = self.0.lock() {
            *current = next.clone();
        }
        let _ = app.emit("startup:state", next);
    }

    pub fn fail(&self, app: &AppHandle, error: AppError) {
        self.publish(
            app,
            StartupState {
                phase: "degraded",
                ready: true,
                progress: 1.0,
                pending_files: 0,
                blocker: Some(error),
                recovery_actions: vec!["open_settings", "retry_startup"],
            },
        );
    }
}

#[tauri::command(async)]
pub fn startup_get_state(startup: State<'_, StartupServiceState>) -> StartupState {
    startup
        .0
        .lock()
        .map(|state| state.clone())
        .unwrap_or_else(|_| StartupState {
            phase: "degraded",
            ready: true,
            progress: 1.0,
            pending_files: 0,
            blocker: Some(AppError::new(
                "STARTUP_STATE_UNAVAILABLE",
                "启动状态暂时无法读取，基础功能仍可继续使用",
                true,
            )),
            recovery_actions: vec!["retry_startup"],
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_state_begins_non_blocking_and_not_ready() {
        let state = StartupState::default();
        assert!(!state.ready);
        assert_eq!(state.phase, "opening_catalog");
        assert!(state.progress > 0.0);
    }
}
