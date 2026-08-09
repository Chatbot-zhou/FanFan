use std::sync::Mutex;

use remin_core::{AppError, WelcomeService};
use serde::Deserialize;
use tauri::State;

pub struct WelcomeServiceState(pub Mutex<WelcomeService>);

#[derive(Debug, Deserialize)]
pub struct WelcomeCompleteRequest {
    pub welcome_version: String,
}

#[tauri::command(async)]
pub fn welcome_get_state(
    service: State<'_, WelcomeServiceState>,
) -> Result<remin_core::welcome::WelcomeState, AppError> {
    service
        .0
        .lock()
        .map_err(|_| AppError::local_config("欢迎状态锁不可用", true))?
        .get_state()
}

#[tauri::command(async)]
pub fn welcome_complete(
    request: WelcomeCompleteRequest,
    service: State<'_, WelcomeServiceState>,
) -> Result<remin_core::welcome::WelcomeState, AppError> {
    service
        .0
        .lock()
        .map_err(|_| AppError::local_config("欢迎状态锁不可用", true))?
        .complete(&request.welcome_version)
}

#[tauri::command(async)]
pub fn welcome_authorization_complete(
    service: State<'_, WelcomeServiceState>,
) -> Result<remin_core::welcome::WelcomeState, AppError> {
    service
        .0
        .lock()
        .map_err(|_| AppError::local_config("欢迎状态锁不可用", true))?
        .complete_root_authorization()
}
