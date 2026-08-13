use std::sync::Mutex;

use fanfan_core::{AppError, ThemePreference, ThemeService, ThemeState};
use serde::Deserialize;
use tauri::State;

pub struct ThemeServiceState(pub Mutex<ThemeService>);

#[derive(Debug, Deserialize)]
pub struct ThemeStateRequest {
    pub system_dark: bool,
}

#[derive(Debug, Deserialize)]
pub struct ThemePreferenceRequest {
    pub preference: ThemePreference,
    pub system_dark: bool,
}

#[tauri::command(async)]
pub fn theme_get_state(
    request: ThemeStateRequest,
    service: State<'_, ThemeServiceState>,
) -> Result<ThemeState, AppError> {
    service
        .0
        .lock()
        .map_err(|_| AppError::local_config("主题状态锁不可用", true))?
        .get_state(request.system_dark)
}

#[tauri::command(async)]
pub fn theme_set_preference(
    request: ThemePreferenceRequest,
    service: State<'_, ThemeServiceState>,
) -> Result<ThemeState, AppError> {
    service
        .0
        .lock()
        .map_err(|_| AppError::local_config("主题状态锁不可用", true))?
        .set_preference(request.preference, request.system_dark)
}
