use std::fs;
use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use stream_download::http::reqwest as sd_reqwest;

pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const LATEST_RELEASE_API: &str =
    "https://api.github.com/repos/sconelli24/YT-Niusic/releases/latest";
const EXPECTED_ASSET_NAME: &str = "yt-niusic.exe";

/// Información de una nueva versión disponible en GitHub Releases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateInfo {
    pub version: String,
    pub html_url: String,
    pub download_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GithubRelease {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<GithubAsset>,
}

#[derive(Debug, Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
}

/// Parsea una versión semántica simple `vX.Y.Z` o `X.Y.Z` (ignorando sufijos `-beta`/`+build`).
pub fn parse_semver(raw: &str) -> Option<(u64, u64, u64)> {
    let cleaned = raw.trim().trim_start_matches(['v', 'V']);
    let core = cleaned.split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse::<u64>().ok()?;
    let minor = parts.next()?.parse::<u64>().ok()?;
    let patch = parts.next().unwrap_or("0").parse::<u64>().ok()?;
    Some((major, minor, patch))
}

/// Devuelve `true` si `latest` es estrictamente mayor que `current`.
pub fn is_newer_version(current: &str, latest: &str) -> bool {
    match (parse_semver(current), parse_semver(latest)) {
        (Some(cur), Some(lat)) => lat > cur,
        _ => false,
    }
}

fn is_trusted_release_url(url: &str) -> bool {
    url.starts_with("https://github.com/sconelli24/YT-Niusic/")
        || url.starts_with("https://objects.githubusercontent.com/")
        || url.starts_with("https://release-assets.githubusercontent.com/")
}

fn parse_update_from_release_json(current_version: &str, body: &str) -> Option<UpdateInfo> {
    let release: GithubRelease = serde_json::from_str(body).ok()?;
    if release.draft || release.prerelease {
        return None;
    }
    if !is_newer_version(current_version, &release.tag_name) {
        return None;
    }
    if !is_trusted_release_url(&release.html_url) {
        return None;
    }

    let download_url = release
        .assets
        .into_iter()
        .find(|a| a.name.eq_ignore_ascii_case(EXPECTED_ASSET_NAME))
        .map(|a| a.browser_download_url)
        .filter(|u| is_trusted_release_url(u));

    let version = release
        .tag_name
        .trim()
        .trim_start_matches(['v', 'V'])
        .to_string();

    Some(UpdateInfo {
        version,
        html_url: release.html_url,
        download_url,
    })
}

/// Consulta silenciosamente GitHub Releases en segundo plano al iniciar la app.
/// Nunca bloquea ni falla ruidosamente si no hay internet.
pub async fn check_for_updates() -> Option<UpdateInfo> {
    let client = sd_reqwest::Client::builder()
        .user_agent(format!("yt-niusic/{CURRENT_VERSION}"))
        .timeout(Duration::from_secs(4))
        .build()
        .ok()?;

    let resp = client
        .get(LATEST_RELEASE_API)
        .header(sd_reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .await
        .ok()?;

    if !resp.status().is_success() {
        return None;
    }

    let body = resp.text().await.ok()?;
    parse_update_from_release_json(CURRENT_VERSION, &body)
}

/// Limpia residuos `.old` / `.new` dejados por una auto-actualización previa.
pub fn cleanup_previous_update_artifacts() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let old_path = exe.with_extension("exe.old");
    let new_path = exe.with_extension("exe.new");
    if old_path.exists() {
        let _ = fs::remove_file(&old_path);
    }
    if new_path.exists() {
        let _ = fs::remove_file(&new_path);
    }
}

/// Descarga el nuevo `yt-niusic.exe` y reemplaza el binario actual de forma segura.
/// En Windows un ejecutable en uso puede renombrarse a `.exe.old` mientras el nuevo ocupa su lugar.
pub async fn download_and_install_update(info: &UpdateInfo) -> Result<String> {
    let Some(download_url) = info.download_url.as_deref() else {
        crate::browser_auth::open_url_in_default_browser(&info.html_url)?;
        return Ok(format!(
            "OPENED RELEASE v{} IN BROWSER",
            info.version
        ));
    };

    if !is_trusted_release_url(download_url) {
        return Err(anyhow!("URL de descarga no confiable"));
    }

    let client = sd_reqwest::Client::builder()
        .user_agent(format!("yt-niusic/{CURRENT_VERSION}"))
        .timeout(Duration::from_secs(60))
        .build()
        .context("No se pudo crear cliente HTTP para descargar la actualización")?;

    let resp = client
        .get(download_url)
        .send()
        .await
        .context("Error al descargar el nuevo ejecutable desde GitHub")?;

    if !resp.status().is_success() {
        return Err(anyhow!(
            "GitHub devolvió estado {} al descargar la actualización",
            resp.status()
        ));
    }

    let bytes = resp
        .bytes()
        .await
        .context("Error leyendo bytes de la actualización")?;

    // Verificación mínima de integridad: binario PE de Windows comienza con "MZ" y tamaño razonable
    if bytes.len() < 100_000 || !bytes.starts_with(b"MZ") {
        return Err(anyhow!(
            "El archivo descargado no parece ser un ejecutable válido de Windows"
        ));
    }

    let current_exe =
        std::env::current_exe().context("No se pudo determinar la ruta del ejecutable actual")?;
    replace_executable_atomic(&current_exe, &bytes)?;

    Ok(format!(
        "UPDATED TO v{} // RESTART YT-NIUSIC TO APPLY",
        info.version
    ))
}

fn replace_executable_atomic(current_exe: &Path, new_bytes: &[u8]) -> Result<()> {
    let new_path = current_exe.with_extension("exe.new");
    let old_path = current_exe.with_extension("exe.old");

    let _ = fs::remove_file(&new_path);
    let _ = fs::remove_file(&old_path);

    fs::write(&new_path, new_bytes).with_context(|| {
        format!(
            "No se pudo escribir el archivo temporal {}",
            new_path.display()
        )
    })?;

    if let Err(e) = fs::rename(current_exe, &old_path) {
        let _ = fs::remove_file(&new_path);
        return Err(anyhow!(
            "No se pudo reemplazar el ejecutable actual ({e}). Descárgalo desde GitHub Releases."
        ));
    }

    if let Err(e) = fs::rename(&new_path, current_exe) {
        // Restaurar respaldo si falló el segundo rename
        let _ = fs::rename(&old_path, current_exe);
        let _ = fs::remove_file(&new_path);
        return Err(anyhow!(
            "No se pudo activar el nuevo ejecutable ({e}); se restauró la versión anterior."
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_comparison_works() {
        assert!(is_newer_version("0.2.1", "v0.2.2"));
        assert!(is_newer_version("0.2.1", "0.3.0"));
        assert!(is_newer_version("0.2.9", "v0.10.0"));
        assert!(is_newer_version("0.2.1", "v1.0.0"));
        assert!(!is_newer_version("0.2.1", "v0.2.1"));
        assert!(!is_newer_version("0.3.0", "v0.2.9"));
        assert!(!is_newer_version("0.3.0", "invalid"));
    }

    #[test]
    fn parses_github_release_and_filters_assets() {
        let json = r#"{
            "tag_name": "v0.4.0",
            "html_url": "https://github.com/sconelli24/YT-Niusic/releases/tag/v0.4.0",
            "draft": false,
            "prerelease": false,
            "assets": [
                {
                    "name": "README.txt",
                    "browser_download_url": "https://github.com/sconelli24/YT-Niusic/releases/download/v0.4.0/README.txt"
                },
                {
                    "name": "yt-niusic.exe",
                    "browser_download_url": "https://github.com/sconelli24/YT-Niusic/releases/download/v0.4.0/yt-niusic.exe"
                }
            ]
        }"#;

        let info = parse_update_from_release_json("0.3.0", json).expect("should detect update");
        assert_eq!(info.version, "0.4.0");
        assert_eq!(
            info.download_url.as_deref(),
            Some("https://github.com/sconelli24/YT-Niusic/releases/download/v0.4.0/yt-niusic.exe")
        );

        // No debe avisar si ya está en la misma versión o más nueva
        assert!(parse_update_from_release_json("0.4.0", json).is_none());
        assert!(parse_update_from_release_json("0.5.0", json).is_none());
    }

    #[test]
    fn ignores_untrusted_urls_and_prereleases() {
        let evil = r#"{
            "tag_name": "v9.9.9",
            "html_url": "https://evil.example.com/release",
            "draft": false,
            "prerelease": false,
            "assets": []
        }"#;
        assert!(parse_update_from_release_json("0.3.0", evil).is_none());

        let pre = r#"{
            "tag_name": "v0.4.0-beta.1",
            "html_url": "https://github.com/sconelli24/YT-Niusic/releases/tag/v0.4.0-beta.1",
            "draft": false,
            "prerelease": true,
            "assets": []
        }"#;
        assert!(parse_update_from_release_json("0.3.0", pre).is_none());
    }
}
