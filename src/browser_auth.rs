use std::collections::HashMap;
use std::future::Future;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use stream_download::http::reqwest as sd_reqwest;
use tokio::net::TcpStream;
use tokio::process::Child;
use tokio::sync::oneshot;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};
use zeroize::Zeroizing;

use crate::security::{
    secure_cleanup_auth_profile, CookieFilterStats, TEMP_AUTH_PROFILE_PREFIX,
};

/// URL oficial de Google Sign-In que redirige directamente a YouTube Music al completar el acceso.
pub const GOOGLE_YTM_LOGIN_URL: &str = "https://accounts.google.com/ServiceLogin?service=youtube&uilel=3&passive=true&continue=https%3A%2F%2Fmusic.youtube.com%2F";

const LOGIN_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const CDP_STARTUP_TIMEOUT: Duration = Duration::from_secs(12);
const POLL_INTERVAL: Duration = Duration::from_millis(750);

/// Navegador Chromium detectado en el equipo para abrir la ventana aislada de Google Sign-In.
#[derive(Debug, Clone)]
pub struct DetectedBrowser {
    pub name: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
struct CdpVersionInfo {
    #[serde(rename = "webSocketDebuggerUrl")]
    web_socket_debugger_url: String,
}

#[derive(Debug, Clone, Deserialize)]
struct CdpTargetInfo {
    #[serde(rename = "type")]
    target_type: String,
    url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CdpCookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    #[serde(default)]
    pub path: String,
}

/// Detecta un navegador basado en Chromium instalado en el sistema (Edge, Chrome, Brave, Vivaldi).
/// También permite forzar una ruta concreta mediante la variable `YT_NIUSIC_BROWSER`.
pub fn detect_chromium_browser() -> Option<DetectedBrowser> {
    if let Ok(custom) = std::env::var("YT_NIUSIC_BROWSER") {
        let p = PathBuf::from(custom.trim());
        if p.is_file() {
            let name = p
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("Browser")
                .to_string();
            return Some(DetectedBrowser { name, path: p });
        }
    }

    #[cfg(windows)]
    {
        let pf86 = std::env::var("ProgramFiles(x86)").ok();
        let pf = std::env::var("ProgramFiles").ok();
        let local = std::env::var("LOCALAPPDATA").ok();

        let candidates: &[(&str, &str)] = &[
            ("Microsoft Edge", r"Microsoft\Edge\Application\msedge.exe"),
            ("Google Chrome", r"Google\Chrome\Application\chrome.exe"),
            (
                "Brave",
                r"BraveSoftware\Brave-Browser\Application\brave.exe",
            ),
            ("Vivaldi", r"Vivaldi\Application\vivaldi.exe"),
        ];

        let roots: Vec<&str> = [pf86.as_deref(), pf.as_deref(), local.as_deref()]
            .into_iter()
            .flatten()
            .collect();

        for (label, rel) in candidates {
            for root in &roots {
                let candidate = Path::new(root).join(rel);
                if candidate.is_file() {
                    return Some(DetectedBrowser {
                        name: (*label).to_string(),
                        path: candidate,
                    });
                }
            }
        }

        // Respaldo buscando en PATH
        for (label, bin) in [
            ("Microsoft Edge", "msedge.exe"),
            ("Google Chrome", "chrome.exe"),
            ("Brave", "brave.exe"),
        ] {
            if let Some(found) = find_in_path(bin) {
                return Some(DetectedBrowser {
                    name: label.to_string(),
                    path: found,
                });
            }
        }

        None
    }

    #[cfg(not(windows))]
    {
        for (label, bin) in [
            ("Google Chrome", "google-chrome"),
            ("Chromium", "chromium"),
            ("Chromium", "chromium-browser"),
            ("Microsoft Edge", "microsoft-edge"),
            ("Brave", "brave-browser"),
        ] {
            if let Some(found) = find_in_path(bin) {
                return Some(DetectedBrowser {
                    name: label.to_string(),
                    path: found,
                });
            }
        }
        None
    }
}

fn find_in_path(binary: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(binary);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Abre una URL HTTPS confiable (por ejemplo `https://www.google.com/device`) en el navegador predeterminado del SO.
pub fn open_url_in_default_browser(url: &str) -> Result<()> {
    if !url.starts_with("https://") {
        return Err(anyhow!("Solo se permite abrir URLs HTTPS confiables"));
    }

    #[cfg(windows)]
    {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("No se pudo abrir el navegador predeterminado de Windows")?;
        Ok(())
    }

    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("No se pudo abrir el navegador predeterminado")?;
        Ok(())
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::process::Command::new("xdg-open")
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("No se pudo abrir el navegador predeterminado")?;
        Ok(())
    }
}

/// Comprueba si un dominio de cookie pertenece a YouTube / YouTube Music.
pub fn is_ytm_cookie_domain(domain: &str) -> bool {
    let d = domain.trim().to_ascii_lowercase();
    matches!(
        d.as_str(),
        ".youtube.com"
            | "youtube.com"
            | "music.youtube.com"
            | ".music.youtube.com"
            | "www.youtube.com"
    )
}

fn domain_priority(domain: &str) -> u8 {
    match domain.trim().to_ascii_lowercase().as_str() {
        "music.youtube.com" | ".music.youtube.com" => 3,
        ".youtube.com" | "youtube.com" => 2,
        "www.youtube.com" => 1,
        _ => 0,
    }
}

/// Comprueba si la URL de una pestaña corresponde a YouTube Music ya cargado (fuera de pantallas de login).
pub fn is_ytm_logged_in_url(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    if !lower.starts_with("https://music.youtube.com") {
        return false;
    }
    !lower.contains("accounts.google.com")
        && !lower.contains("accounts.youtube.com")
        && !lower.contains("consent.youtube.com")
        && !lower.contains("/signin")
}

/// Verifica si el conjunto de cookies de YouTube contiene los identificadores mínimos de una sesión activa.
pub fn has_required_ytm_auth_cookies(cookies: &[CdpCookie]) -> bool {
    let mut has_sapisid = false;
    let mut has_sid = false;
    let mut has_identity = false;

    for c in cookies {
        if !is_ytm_cookie_domain(&c.domain) || c.value.trim().is_empty() {
            continue;
        }
        match c.name.as_str() {
            "SAPISID" => has_sapisid = true,
            "SID" | "__Secure-3PSID" | "__Secure-1PSID" => has_sid = true,
            "LOGIN_INFO" | "__Secure-3PAPISID" | "__Secure-1PAPISID" | "HSID" => {
                has_identity = true;
            }
            _ => {}
        }
    }

    has_sapisid && has_sid && has_identity
}

/// Construye una cabecera `Cookie` (protegida con `Zeroizing`) a partir de cookies CDP,
/// conservando solo dominios de YouTube y resolviendo duplicados en favor de `music.youtube.com` / `.youtube.com`.
pub fn build_cookie_header_from_cdp(cookies: &[CdpCookie]) -> Zeroizing<String> {
    let mut best_by_name: HashMap<&str, (u8, &str)> = HashMap::new();

    for c in cookies {
        let prio = domain_priority(&c.domain);
        if prio == 0 {
            continue;
        }
        let name = c.name.trim();
        let val = c.value.trim();
        if name.is_empty() || val.is_empty() {
            continue;
        }
        match best_by_name.get(name) {
            Some((existing_prio, _)) if *existing_prio > prio => {}
            _ => {
                best_by_name.insert(name, (prio, val));
            }
        }
    }

    let mut pairs: Vec<(&str, &str)> = best_by_name
        .into_iter()
        .map(|(name, (_, val))| (name, val))
        .collect();
    pairs.sort_unstable_by_key(|(name, _)| *name);

    let header = pairs
        .into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("; ");

    Zeroizing::new(header)
}

/// Cliente mínimo de Chrome DevTools Protocol sobre WebSocket local (`127.0.0.1`).
struct CdpClient {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    next_id: u64,
}

impl CdpClient {
    async fn connect(ws_url: &str) -> Result<Self> {
        let (ws, _) = connect_async(ws_url)
            .await
            .context("No se pudo conectar al canal local CDP del navegador")?;
        Ok(Self { ws, next_id: 1 })
    }

    async fn call(&mut self, method: &str) -> Result<serde_json::Value> {
        let id = self.next_id;
        self.next_id += 1;

        let payload = serde_json::json!({
            "id": id,
            "method": method,
            "params": {}
        })
        .to_string();

        self.ws
            .send(Message::Text(payload.into()))
            .await
            .with_context(|| format!("Error al enviar comando CDP {method}"))?;

        let deadline = Instant::now() + Duration::from_secs(6);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(anyhow!("Tiempo de espera agotado en comando CDP {method}"));
            }

            let next_msg = tokio::time::timeout(remaining, self.ws.next())
                .await
                .map_err(|_| anyhow!("Timeout leyendo respuesta CDP {method}"))?;

            match next_msg {
                Some(Ok(Message::Text(txt))) => {
                    let parsed: serde_json::Value = match serde_json::from_str(txt.as_str()) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    if parsed.get("id").and_then(|v| v.as_u64()) != Some(id) {
                        continue;
                    }
                    if let Some(err) = parsed.get("error") {
                        return Err(anyhow!("Error CDP en {method}: {err}"));
                    }
                    return Ok(parsed
                        .get("result")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null));
                }
                Some(Ok(Message::Ping(data))) => {
                    let _ = self.ws.send(Message::Pong(data)).await;
                }
                Some(Ok(Message::Close(_))) | None => {
                    return Err(anyhow!("La conexión CDP con el navegador se cerró"));
                }
                Some(Err(e)) => {
                    return Err(anyhow!("Error WebSocket CDP: {e}"));
                }
                _ => {}
            }
        }
    }

    async fn get_targets(&mut self) -> Result<Vec<CdpTargetInfo>> {
        let res = self.call("Target.getTargets").await?;
        let infos = res
            .get("targetInfos")
            .cloned()
            .unwrap_or_else(|| serde_json::Value::Array(Vec::new()));
        serde_json::from_value(infos).context("Respuesta inesperada de Target.getTargets")
    }

    async fn get_cookies(&mut self) -> Result<Vec<CdpCookie>> {
        let res = match self.call("Storage.getCookies").await {
            Ok(v) => v,
            Err(_) => self.call("Network.getAllCookies").await?,
        };
        let cookies = res
            .get("cookies")
            .cloned()
            .unwrap_or_else(|| serde_json::Value::Array(Vec::new()));
        serde_json::from_value(cookies).context("Respuesta inesperada al leer cookies CDP")
    }

    async fn close_browser(&mut self) {
        let _ = tokio::time::timeout(Duration::from_secs(2), self.call("Browser.close")).await;
        let _ = self.ws.close(None).await;
    }
}

fn reserve_free_loopback_port() -> Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .context("No se pudo reservar un puerto local libre para el inicio de sesión")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

fn create_ephemeral_profile_dir(data_dir: &Path) -> Result<PathBuf> {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let dir = data_dir.join(format!(
        "{TEMP_AUTH_PROFILE_PREFIX}{}_{ts}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("No se pudo crear el perfil temporal en {}", dir.display()))?;
    Ok(dir)
}

async fn teardown_browser_and_profile(
    mut cdp: Option<CdpClient>,
    mut child: Child,
    profile_dir: PathBuf,
) {
    if let Some(client) = cdp.as_mut() {
        client.close_browser().await;
    }
    drop(cdp);

    // Esperamos brevemente a que el navegador cierre limpiamente antes de forzar kill
    let exited = tokio::time::timeout(Duration::from_millis(1500), child.wait()).await;
    if exited.is_err() {
        let _ = child.kill().await;
        let _ = tokio::time::timeout(Duration::from_millis(1000), child.wait()).await;
    }

    let _ = tokio::task::spawn_blocking(move || {
        secure_cleanup_auth_profile(&profile_dir);
    })
    .await;
}

/// Abre una ventana aislada del navegador instalado en la página oficial de Google Sign-In,
/// espera a que el usuario complete el inicio de sesión en `music.youtube.com`, valida y guarda
/// la sesión mediante `validate_cookie`, y destruye el perfil temporal al terminar.
pub async fn login_via_isolated_browser<F, V, Fut>(
    data_dir: &Path,
    mut cancel_rx: oneshot::Receiver<()>,
    mut on_status: F,
    mut validate_cookie: V,
) -> Result<CookieFilterStats>
where
    F: FnMut(String),
    V: FnMut(String) -> Fut,
    Fut: Future<Output = Result<CookieFilterStats>>,
{
    let browser = detect_chromium_browser().ok_or_else(|| {
        anyhow!(
            "No se encontró Edge, Chrome ni Brave instalado. Usa OAuth TV [o] o importa cookies [i]/[c]."
        )
    })?;

    let port = reserve_free_loopback_port()?;
    let profile_dir = create_ephemeral_profile_dir(data_dir)?;

    on_status(format!(
        "LAUNCHING {} // COMPLETE GOOGLE SIGN-IN... [ESC TO CANCEL]",
        browser.name.to_uppercase()
    ));

    let mut cmd = tokio::process::Command::new(&browser.path);
    cmd.args([
        format!("--app={GOOGLE_YTM_LOGIN_URL}"),
        format!("--user-data-dir={}", profile_dir.display()),
        format!("--remote-debugging-port={port}"),
        "--remote-debugging-address=127.0.0.1".to_string(),
        "--no-first-run".to_string(),
        "--no-default-browser-check".to_string(),
        "--disable-sync".to_string(),
        "--disable-extensions".to_string(),
        "--disable-background-networking".to_string(),
        "--window-size=520,720".to_string(),
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let p = profile_dir.clone();
            let _ = tokio::task::spawn_blocking(move || secure_cleanup_auth_profile(&p)).await;
            return Err(anyhow!("No se pudo abrir {}: {e}", browser.name));
        }
    };

    let http = sd_reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()
        .context("No se pudo crear el cliente HTTP local para CDP")?;

    let version_url = format!("http://127.0.0.1:{port}/json/version");
    let startup_deadline = Instant::now() + CDP_STARTUP_TIMEOUT;
    let mut ws_url: Option<String> = None;

    while Instant::now() < startup_deadline {
        if cancel_rx.try_recv().is_ok() {
            teardown_browser_and_profile(None, child, profile_dir).await;
            return Err(anyhow!("Inicio de sesión cancelado por el usuario"));
        }
        if let Ok(Some(_)) = child.try_wait() {
            teardown_browser_and_profile(None, child, profile_dir).await;
            return Err(anyhow!(
                "La ventana de {} se cerró antes de completar el inicio de sesión",
                browser.name
            ));
        }

        if let Ok(resp) = http.get(&version_url).send().await
            && resp.status().is_success()
            && let Ok(body) = resp.text().await
            && let Ok(info) = serde_json::from_str::<CdpVersionInfo>(&body)
            && !info.web_socket_debugger_url.is_empty()
        {
            ws_url = Some(info.web_socket_debugger_url);
            break;
        }

        tokio::select! {
            _ = &mut cancel_rx => {
                teardown_browser_and_profile(None, child, profile_dir).await;
                return Err(anyhow!("Inicio de sesión cancelado por el usuario"));
            }
            _ = tokio::time::sleep(Duration::from_millis(250)) => {}
        }
    }

    let Some(ws_url) = ws_url else {
        teardown_browser_and_profile(None, child, profile_dir).await;
        return Err(anyhow!(
            "No se pudo conectar con la ventana de {} (CDP loopback no respondió)",
            browser.name
        ));
    };

    let mut cdp = match CdpClient::connect(&ws_url).await {
        Ok(c) => c,
        Err(e) => {
            teardown_browser_and_profile(None, child, profile_dir).await;
            return Err(e);
        }
    };

    on_status(format!(
        "WAITING FOR GOOGLE SIGN-IN ({})... [ESC TO CANCEL]",
        browser.name.to_uppercase()
    ));

    let login_deadline = Instant::now() + LOGIN_TIMEOUT;
    let mut validation_attempts = 0u8;

    let result: Result<CookieFilterStats> = loop {
        if Instant::now() >= login_deadline {
            break Err(anyhow!(
                "Tiempo agotado (5 min) esperando el inicio de sesión en Google"
            ));
        }

        if let Ok(Some(_)) = child.try_wait() {
            break Err(anyhow!(
                "Inicio de sesión cancelado (se cerró la ventana del navegador)"
            ));
        }

        let targets = match cdp.get_targets().await {
            Ok(t) => t,
            Err(_) => {
                break Err(anyhow!(
                    "Inicio de sesión cancelado (la ventana del navegador se cerró)"
                ));
            }
        };

        let on_ytm = targets
            .iter()
            .any(|t| t.target_type == "page" && is_ytm_logged_in_url(&t.url));

        if on_ytm {
            let cookies = match cdp.get_cookies().await {
                Ok(c) => c,
                Err(_) => {
                    break Err(anyhow!(
                        "No se pudieron leer las cookies de sesión desde el navegador"
                    ));
                }
            };

            if has_required_ytm_auth_cookies(&cookies) {
                on_status("GOOGLE SIGN-IN DETECTED // VALIDATING & ENCRYPTING DPAPI...".into());

                // Breve pausa para que music.youtube.com termine de asentar cookies finales
                tokio::select! {
                    _ = &mut cancel_rx => {
                        break Err(anyhow!("Inicio de sesión cancelado por el usuario"));
                    }
                    _ = tokio::time::sleep(Duration::from_millis(600)) => {}
                }

                let settled_cookies = cdp.get_cookies().await.unwrap_or(cookies);
                let header = build_cookie_header_from_cdp(&settled_cookies);

                match validate_cookie(header.to_string()).await {
                    Ok(stats) => break Ok(stats),
                    Err(e) => {
                        validation_attempts += 1;
                        if validation_attempts >= 6 {
                            break Err(anyhow!(
                                "No se pudo validar la sesión capturada tras varios intentos: {e}"
                            ));
                        }
                    }
                }
            }
        }

        tokio::select! {
            _ = &mut cancel_rx => {
                break Err(anyhow!("Inicio de sesión cancelado por el usuario"));
            }
            _ = tokio::time::sleep(POLL_INTERVAL) => {}
        }
    };

    teardown_browser_and_profile(Some(cdp), child, profile_dir).await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener as TokioTcpListener;
    use tokio_tungstenite::accept_async;

    fn mk_cookie(domain: &str, name: &str, value: &str) -> CdpCookie {
        CdpCookie {
            name: name.to_string(),
            value: value.to_string(),
            domain: domain.to_string(),
            path: "/".to_string(),
        }
    }

    #[test]
    fn ytm_url_detection_ignores_accounts_pages() {
        assert!(is_ytm_logged_in_url("https://music.youtube.com/"));
        assert!(is_ytm_logged_in_url("https://music.youtube.com/library"));
        assert!(!is_ytm_logged_in_url(
            "https://accounts.google.com/ServiceLogin?continue=https://music.youtube.com/"
        ));
        assert!(!is_ytm_logged_in_url(
            "https://accounts.youtube.com/accounts/SetSID"
        ));
        assert!(!is_ytm_logged_in_url(
            "https://music.youtube.com/signin?next=/"
        ));
        assert!(!is_ytm_logged_in_url("https://www.google.com/"));
    }

    #[test]
    fn cdp_cookie_filtering_and_domain_priority() {
        let cookies = vec![
            mk_cookie(".google.com", "NID", "tracking"),
            mk_cookie(".youtube.com", "SAPISID", "root_sapisid"),
            mk_cookie("music.youtube.com", "SAPISID", "music_sapisid"),
            mk_cookie(".youtube.com", "SID", "sid_val"),
            mk_cookie(".youtube.com", "LOGIN_INFO", "login_val"),
            mk_cookie(".youtube.com", "EMPTY", "   "),
            mk_cookie("evil.example.com", "SAPISID", "spoofed"),
        ];

        assert!(has_required_ytm_auth_cookies(&cookies));

        let header = build_cookie_header_from_cdp(&cookies);
        assert!(header.contains("SAPISID=music_sapisid"));
        assert!(header.contains("SID=sid_val"));
        assert!(header.contains("LOGIN_INFO=login_val"));
        assert!(!header.contains("tracking"));
        assert!(!header.contains("spoofed"));
        assert!(!header.contains("EMPTY="));
    }

    #[test]
    fn missing_required_cookies_returns_false() {
        let incomplete = vec![
            mk_cookie(".youtube.com", "VISITOR_INFO1_LIVE", "abc"),
            mk_cookie(".youtube.com", "SOCS", "xyz"),
        ];
        assert!(!has_required_ytm_auth_cookies(&incomplete));
    }

    #[tokio::test]
    async fn cdp_client_roundtrip_over_loopback() {
        let listener = TokioTcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();

            // Enviamos un evento no solicitado primero para verificar que CdpClient lo ignora
            ws.send(Message::Text(
                r#"{"method":"Target.targetCreated","params":{}}"#.into(),
            ))
            .await
            .unwrap();

            while let Some(Ok(Message::Text(txt))) = ws.next().await {
                let req: serde_json::Value = serde_json::from_str(txt.as_str()).unwrap();
                let id = req["id"].as_u64().unwrap();
                let method = req["method"].as_str().unwrap();
                let resp = match method {
                    "Target.getTargets" => serde_json::json!({
                        "id": id,
                        "result": {
                            "targetInfos": [
                                { "type": "page", "url": "https://music.youtube.com/" }
                            ]
                        }
                    }),
                    "Storage.getCookies" => serde_json::json!({
                        "id": id,
                        "result": {
                            "cookies": [
                                { "name": "SAPISID", "value": "sap123", "domain": ".youtube.com", "path": "/" },
                                { "name": "SID", "value": "sid123", "domain": ".youtube.com", "path": "/" },
                                { "name": "LOGIN_INFO", "value": "li123", "domain": ".youtube.com", "path": "/" }
                            ]
                        }
                    }),
                    "Browser.close" => {
                        let _ = ws
                            .send(Message::Text(
                                serde_json::json!({ "id": id, "result": {} })
                                    .to_string()
                                    .into(),
                            ))
                            .await;
                        break;
                    }
                    _ => serde_json::json!({ "id": id, "result": {} }),
                };
                ws.send(Message::Text(resp.to_string().into()))
                    .await
                    .unwrap();
            }
        });

        let ws_url = format!("ws://{addr}/devtools/browser/test");
        let mut client = CdpClient::connect(&ws_url).await.unwrap();

        let targets = client.get_targets().await.unwrap();
        assert_eq!(targets.len(), 1);
        assert!(is_ytm_logged_in_url(&targets[0].url));

        let cookies = client.get_cookies().await.unwrap();
        assert!(has_required_ytm_auth_cookies(&cookies));

        client.close_browser().await;
        server.await.unwrap();
    }
}
