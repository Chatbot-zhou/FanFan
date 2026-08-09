use std::{
    fs,
    path::{Path, PathBuf},
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::AppError;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WelcomeState {
    pub welcome_version: String,
    pub welcome_completed: bool,
    pub welcome_completed_at: Option<DateTime<Utc>>,
}

impl WelcomeState {
    fn initial(version: &str) -> Self {
        Self {
            welcome_version: version.to_owned(),
            welcome_completed: false,
            welcome_completed_at: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct WelcomeService {
    state_file: PathBuf,
    current_version: String,
}

impl WelcomeService {
    pub fn new(config_dir: impl Into<PathBuf>, current_version: impl Into<String>) -> Self {
        Self {
            state_file: config_dir.into().join("welcome.json"),
            current_version: current_version.into(),
        }
    }

    pub fn get_state(&self) -> Result<WelcomeState, AppError> {
        if !self.state_file.exists() {
            return Ok(WelcomeState::initial(&self.current_version));
        }
        let bytes = fs::read(&self.state_file)
            .map_err(|error| AppError::local_config(error.to_string(), true))?;
        match serde_json::from_slice(&bytes) {
            Ok(state) => Ok(state),
            Err(_) => Ok(WelcomeState::initial(&self.current_version)),
        }
    }

    pub fn complete(&self, requested_version: &str) -> Result<WelcomeState, AppError> {
        if requested_version != self.current_version {
            return Err(AppError {
                code: "WELCOME_VERSION_MISMATCH".into(),
                message: "欢迎页版本与当前应用不一致".into(),
                retryable: false,
                user_action: Some("请重新启动拾忆".into()),
                file_id: None,
                details: None,
            });
        }
        if let Ok(current) = self.get_state()
            && current.welcome_completed
            && current.welcome_version == requested_version
        {
            return Ok(current);
        }
        let completed = WelcomeState {
            welcome_version: requested_version.to_owned(),
            welcome_completed: true,
            welcome_completed_at: Some(Utc::now()),
        };
        self.atomic_write(&completed)?;
        Ok(completed)
    }

    fn atomic_write(&self, state: &WelcomeState) -> Result<(), AppError> {
        let parent = self.state_file.parent().unwrap_or(Path::new("."));
        fs::create_dir_all(parent)
            .map_err(|error| AppError::local_config(error.to_string(), true))?;
        let temp = self.state_file.with_extension("json.new");
        let bytes = serde_json::to_vec_pretty(state)
            .map_err(|error| AppError::local_config(error.to_string(), false))?;
        fs::write(&temp, bytes).map_err(|error| AppError::local_config(error.to_string(), true))?;
        if self.state_file.exists() {
            fs::remove_file(&self.state_file)
                .map_err(|error| AppError::local_config(error.to_string(), true))?;
        }
        fs::rename(&temp, &self.state_file)
            .map_err(|error| AppError::local_config(error.to_string(), true))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_is_persisted_and_idempotent() {
        let directory = tempfile::tempdir().expect("tempdir");
        let service = WelcomeService::new(directory.path(), "1.0");
        assert!(
            !service
                .get_state()
                .expect("initial state")
                .welcome_completed
        );
        let first = service.complete("1.0").expect("complete");
        let second = service.complete("1.0").expect("idempotent complete");
        assert!(first.welcome_completed);
        assert_eq!(first, second);
    }

    #[test]
    fn mismatched_version_is_rejected() {
        let directory = tempfile::tempdir().expect("tempdir");
        let service = WelcomeService::new(directory.path(), "1.0");
        assert_eq!(
            service.complete("2.0").unwrap_err().code,
            "WELCOME_VERSION_MISMATCH"
        );
    }
}
