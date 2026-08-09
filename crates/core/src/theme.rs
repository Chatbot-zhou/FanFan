use std::{
    fs,
    path::{Path, PathBuf},
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::AppError;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ThemePreference {
    #[default]
    System,
    DayGradient,
    NightDark,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EffectiveTheme {
    DayGradient,
    NightDark,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ThemeState {
    pub preference: ThemePreference,
    pub effective_theme: EffectiveTheme,
    pub updated_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
struct StoredThemeSettings {
    #[serde(default)]
    preference: ThemePreference,
    updated_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub struct ThemeService {
    state_file: PathBuf,
}

impl ThemeService {
    pub fn new(config_dir: impl Into<PathBuf>) -> Self {
        Self {
            state_file: config_dir.into().join("theme.json"),
        }
    }

    pub fn get_state(&self, system_dark: bool) -> Result<ThemeState, AppError> {
        let stored = self.read_settings()?;
        Ok(to_state(stored, system_dark))
    }

    pub fn set_preference(
        &self,
        preference: ThemePreference,
        system_dark: bool,
    ) -> Result<ThemeState, AppError> {
        let stored = StoredThemeSettings {
            preference,
            updated_at: Some(Utc::now()),
        };
        self.atomic_write(&stored)?;
        Ok(to_state(stored, system_dark))
    }

    fn read_settings(&self) -> Result<StoredThemeSettings, AppError> {
        if !self.state_file.exists() {
            return Ok(StoredThemeSettings::default());
        }
        let bytes = fs::read(&self.state_file)
            .map_err(|error| AppError::local_config(error.to_string(), true))?;
        serde_json::from_slice(&bytes)
            .map_err(|error| AppError::local_config(error.to_string(), true))
    }

    fn atomic_write(&self, settings: &StoredThemeSettings) -> Result<(), AppError> {
        let parent = self.state_file.parent().unwrap_or(Path::new("."));
        fs::create_dir_all(parent)
            .map_err(|error| AppError::local_config(error.to_string(), true))?;
        let temporary = self.state_file.with_extension("json.new");
        let bytes = serde_json::to_vec_pretty(settings)
            .map_err(|error| AppError::local_config(error.to_string(), false))?;
        fs::write(&temporary, bytes)
            .map_err(|error| AppError::local_config(error.to_string(), true))?;
        if self.state_file.exists() {
            fs::remove_file(&self.state_file)
                .map_err(|error| AppError::local_config(error.to_string(), true))?;
        }
        fs::rename(&temporary, &self.state_file)
            .map_err(|error| AppError::local_config(error.to_string(), true))
    }
}

fn to_state(settings: StoredThemeSettings, system_dark: bool) -> ThemeState {
    let effective_theme = match settings.preference {
        ThemePreference::System if system_dark => EffectiveTheme::NightDark,
        ThemePreference::NightDark => EffectiveTheme::NightDark,
        ThemePreference::System | ThemePreference::DayGradient => EffectiveTheme::DayGradient,
    };
    ThemeState {
        preference: settings.preference,
        effective_theme,
        updated_at: settings.updated_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_theme_follows_windows_and_manual_choice_persists() {
        let directory = tempfile::tempdir().expect("tempdir");
        let service = ThemeService::new(directory.path());
        assert_eq!(
            service.get_state(false).expect("default").effective_theme,
            EffectiveTheme::DayGradient
        );
        assert_eq!(
            service
                .get_state(true)
                .expect("system dark")
                .effective_theme,
            EffectiveTheme::NightDark
        );
        service
            .set_preference(ThemePreference::DayGradient, true)
            .expect("persist manual theme");
        assert_eq!(
            service
                .get_state(true)
                .expect("manual wins")
                .effective_theme,
            EffectiveTheme::DayGradient
        );
    }
}
