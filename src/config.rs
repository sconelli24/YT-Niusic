use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

/// Preferencias no sensibles de la aplicación.
/// Las credenciales/cookies de Google NUNCA se guardan aquí ni en el repositorio;
/// se almacenan exclusivamente en el directorio de datos privado del usuario del SO.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppSettings {
    pub volume: f32,
    pub autoplay_enabled: bool,
    pub normalize_loudness: bool,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            volume: 0.85,
            autoplay_enabled: true,
            normalize_loudness: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AppPaths {
    /// Directorio privado en %APPDATA% / XDG_DATA_HOME para caché y sesión de RustyPipe.
    pub data_dir: PathBuf,
    /// Archivo de preferencias ligeras de la UI.
    pub settings_file: PathBuf,
}

impl AppPaths {
    pub fn resolve() -> Result<Self> {
        let proj = ProjectDirs::from("com", "yt-niusic", "YT-Niusic")
            .context("No se pudo determinar el directorio de datos del sistema")?;

        let data_dir = proj.data_local_dir().to_path_buf();
        let config_dir = proj.config_dir().to_path_buf();

        fs::create_dir_all(&data_dir).with_context(|| {
            format!(
                "No se pudo crear el directorio de datos: {}",
                data_dir.display()
            )
        })?;
        fs::create_dir_all(&config_dir).with_context(|| {
            format!(
                "No se pudo crear el directorio de configuración: {}",
                config_dir.display()
            )
        })?;

        let settings_file = config_dir.join("settings.json");

        Ok(Self {
            data_dir,
            settings_file,
        })
    }

    pub fn load_settings(&self) -> AppSettings {
        read_settings(&self.settings_file).unwrap_or_default()
    }

    pub fn save_settings(&self, settings: &AppSettings) {
        if let Ok(json) = serde_json::to_string_pretty(settings) {
            let _ = fs::write(&self.settings_file, json);
        }
    }
}

fn read_settings(path: &Path) -> Option<AppSettings> {
    let raw = fs::read_to_string(path).ok()?;
    let mut parsed: AppSettings = serde_json::from_str(&raw).ok()?;
    parsed.volume = parsed.volume.clamp(0.0, 1.0);
    Some(parsed)
}
