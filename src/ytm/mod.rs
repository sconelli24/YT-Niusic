use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use rustypipe::client::{ClientType, OauthDeviceCode, RustyPipe, RustyPipeQuery};
use rustypipe::model::paginator::ContinuationEndpoint;
use rustypipe::model::{
    AlbumItem, AlbumType, AudioCodec, AudioStream, AudioTrackType, MusicAlbum, Thumbnail,
    TrackItem,
};
use stream_download::http::reqwest as sd_reqwest;
use zeroize::Zeroizing;

use crate::security::{
    filter_minimal_ytm_cookies, secure_delete_file, CookieFilterStats, EncryptedCacheStorage,
};

/// Canción normalizada de YouTube Music.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Track {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub duration_secs: Option<u32>,
    pub cover_url: Option<String>,
}

/// Resumen de un álbum devuelto por búsqueda o biblioteca.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlbumSummary {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub year: Option<u16>,
    pub album_type: String,
    pub cover_url: Option<String>,
}

/// Detalle completo de un álbum con todas sus pistas.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlbumDetail {
    pub summary: AlbumSummary,
    pub tracks: Vec<Track>,
    pub playlist_id: Option<String>,
}

/// Página de recomendaciones de Radio / "A continuación" de Google.
#[derive(Debug, Clone)]
pub struct RadioPage {
    pub tracks: Vec<Track>,
    pub continuation: Option<String>,
    pub visitor_data: Option<String>,
}

/// Stream de audio de alta calidad listo para reproducir.
#[derive(Debug, Clone)]
pub struct ResolvedAudio {
    pub url: String,
    pub user_agent: String,
    pub bitrate_kbps: u32,
    pub codec_label: String,
    pub loudness_db: Option<f32>,
    pub duration_secs: Option<u32>,
    pub content_length: Option<u64>,
}

/// Estado local de vinculación de cuenta (sin exponer ningún secreto).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AuthStatus {
    pub has_cookie: bool,
    pub has_oauth: bool,
}

impl AuthStatus {
    pub fn is_personalized(&self) -> bool {
        self.has_cookie
    }
}

/// Información visible del flujo OAuth por código de dispositivo (Google TV).
#[derive(Clone)]
pub struct OAuthSession {
    pub code: Arc<OauthDeviceCode>,
}

impl std::fmt::Debug for OAuthSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthSession")
            .field("verification_url", &self.code.verification_url)
            .finish_non_exhaustive()
    }
}

/// Cliente principal para YouTube Music.
///
/// Privacidad:
/// - Almacenamiento cifrado con Windows DPAPI (`EncryptedCacheStorage`, vinculado a tu usuario de Windows).
/// - Desactiva reportes en disco (`.no_reporter()`).
/// - Filtra cookies para conservar únicamente las estrictamente necesarias para `music.youtube.com`.
/// - Nunca imprime cookies ni tokens en logs.
#[derive(Clone)]
pub struct YtmClient {
    rp: RustyPipe,
}

impl YtmClient {
    pub fn new(storage_dir: &Path) -> Result<Self> {
        let encrypted_storage = EncryptedCacheStorage::new(storage_dir);
        let rp = RustyPipe::builder()
            .storage_dir(storage_dir)
            .storage(Box::new(encrypted_storage))
            .no_reporter()
            .timezone_local()
            .build()
            .context("No se pudo inicializar el cliente de YouTube Music")?;

        Ok(Self { rp })
    }

    /// Devuelve el estado de autenticación local sin consultar la red.
    pub fn auth_status(&self) -> AuthStatus {
        let q = self.rp.query();
        AuthStatus {
            has_cookie: q.auth_enabled(ClientType::DesktopMusic),
            has_oauth: q.auth_enabled(ClientType::Tv),
        }
    }

    /// Importa cookies desde un archivo `cookies.txt` (formato Netscape) o texto plano de cabecera `Cookie`.
    /// Si `delete_after_import` es `true`, sobrescribe con ceros y elimina el archivo original al validar la sesión.
    pub async fn login_with_cookie_file(
        &self,
        path: &Path,
        delete_after_import: bool,
    ) -> Result<CookieFilterStats> {
        let content = Zeroizing::new(
            tokio::fs::read_to_string(path)
                .await
                .context("No se pudo leer el archivo de cookies seleccionado")?,
        );
        let stats = self.login_with_cookie_input(&content).await?;
        if delete_after_import {
            let _ = secure_delete_file(path);
        }
        Ok(stats)
    }

    /// Configura la sesión con cookies pegadas o leídas de archivo, filtrando primero a mínimo privilegio.
    pub async fn login_with_cookie_input(&self, raw: &str) -> Result<CookieFilterStats> {
        if raw.trim().is_empty() {
            return Err(anyhow!("El contenido de cookies está vacío"));
        }

        let (filtered, stats) = filter_minimal_ytm_cookies(raw);
        if stats.kept == 0 {
            return Err(anyhow!(
                "No se encontraron cookies válidas de YouTube Music (asegúrate de exportar/copiar desde music.youtube.com con sesión iniciada)"
            ));
        }

        let res = if stats.is_netscape {
            self.rp.user_auth_set_cookie_txt(&filtered).await
        } else {
            self.rp.user_auth_set_cookie(filtered.as_str()).await
        };

        res.map_err(|e| {
            anyhow!(
                "No se pudo validar la sesión con esas cookies (asegúrate de haber iniciado sesión en music.youtube.com): {e}"
            )
        })?;

        Ok(stats)
    }

    /// Verifica si las cookies guardadas siguen siendo válidas en Google.
    pub async fn verify_cookie_session(&self) -> Result<()> {
        self.rp
            .user_auth_check_cookie()
            .await
            .map_err(|e| anyhow!("La sesión de cookies expiró o no es válida: {e}"))
    }

    /// Elimina las cookies guardadas localmente.
    pub async fn logout_cookie(&self) -> Result<()> {
        let _ = self.rp.user_auth_remove_cookie().await;
        Ok(())
    }

    /// Inicia login OAuth por código de dispositivo (útil para reproducción autenticada sin cookies).
    pub async fn start_oauth(&self) -> Result<OAuthSession> {
        let code = self
            .rp
            .user_auth_get_code()
            .await
            .context("No se pudo solicitar el código de dispositivo OAuth a Google")?;
        Ok(OAuthSession {
            code: Arc::new(code),
        })
    }

    /// Comprueba si el usuario completó la autorización OAuth en google.com/device.
    pub async fn poll_oauth(&self, session: &OAuthSession) -> Result<bool> {
        self.rp
            .user_auth_login(&session.code)
            .await
            .map_err(|e| anyhow!("Error al comprobar autorización OAuth: {e}"))
    }

    /// Revoca y elimina el token OAuth local.
    pub async fn logout_oauth(&self) -> Result<()> {
        let _ = self.rp.user_auth_logout().await;
        Ok(())
    }

    /// Cierra completamente la sesión local (cookies y OAuth TV) eliminando los secretos del almacenamiento DPAPI.
    pub async fn logout_all(&self) -> Result<()> {
        let _ = self.rp.user_auth_remove_cookie().await;
        let _ = self.rp.user_auth_logout().await;
        Ok(())
    }

    /// Inicia sesión interactivamente abriendo la página oficial de Google en una ventana aislada
    /// del navegador instalado, captura la sesión de YouTube Music y la guarda cifrada con Windows DPAPI.
    pub async fn login_with_browser<F>(
        &self,
        data_dir: &Path,
        cancel_rx: tokio::sync::oneshot::Receiver<()>,
        on_status: F,
    ) -> Result<CookieFilterStats>
    where
        F: FnMut(String),
    {
        let client = self.clone();
        crate::browser_auth::login_via_isolated_browser(
            data_dir,
            cancel_rx,
            on_status,
            move |cookie_header| {
                let c = client.clone();
                async move {
                    let raw = Zeroizing::new(cookie_header);
                    c.login_with_cookie_input(&raw).await
                }
            },
        )
        .await
    }

    /// Construye un query autenticado si hay cookies de cuenta disponibles.
    fn music_query(&self) -> RustyPipeQuery {
        let q = self.rp.query();
        if q.auth_enabled(ClientType::DesktopMusic) {
            q.authenticated()
        } else {
            q
        }
    }

    /// Busca canciones en YouTube Music.
    pub async fn search_songs(&self, query: &str) -> Result<Vec<Track>> {
        let q = query.trim();
        if q.is_empty() {
            return Ok(Vec::new());
        }
        let res = self
            .music_query()
            .music_search_tracks(q)
            .await
            .context("Error al buscar canciones en YouTube Music")?;

        Ok(res.items.items.into_iter().map(map_track).collect())
    }

    /// Busca álbumes en YouTube Music.
    pub async fn search_albums(&self, query: &str) -> Result<Vec<AlbumSummary>> {
        let q = query.trim();
        if q.is_empty() {
            return Ok(Vec::new());
        }
        let res = self
            .music_query()
            .music_search_albums(q)
            .await
            .context("Error al buscar álbumes en YouTube Music")?;

        Ok(res.items.items.into_iter().map(map_album_item).collect())
    }

    /// Obtiene el detalle completo y todas las canciones de un álbum.
    pub async fn get_album(&self, album_id: &str) -> Result<AlbumDetail> {
        let album = self
            .music_query()
            .music_album(album_id)
            .await
            .context("Error al cargar el álbum de YouTube Music")?;

        Ok(map_album_detail(album))
    }

    /// Obtiene canciones con "Me gusta" de la cuenta vinculada (requiere cookies).
    pub async fn get_liked_tracks(&self) -> Result<Vec<Track>> {
        let playlist = self
            .rp
            .query()
            .music_liked_tracks()
            .await
            .context("No se pudieron cargar tus canciones con Me gusta (requiere sesión con cookies)")?;

        Ok(playlist.tracks.items.into_iter().map(map_track).collect())
    }

    /// Obtiene el historial reciente de YouTube Music de la cuenta vinculada (requiere cookies).
    pub async fn get_history(&self) -> Result<Vec<Track>> {
        let page = self
            .rp
            .query()
            .music_history()
            .await
            .context("No se pudo cargar tu historial de YouTube Music (requiere sesión con cookies)")?;

        Ok(page.items.into_iter().map(|h| map_track(h.item)).collect())
    }

    /// Obtiene álbumes guardados de la cuenta vinculada (requiere cookies).
    pub async fn get_saved_albums(&self) -> Result<Vec<AlbumSummary>> {
        let page = self
            .rp
            .query()
            .music_saved_albums()
            .await
            .context("No se pudieron cargar tus álbumes guardados (requiere sesión con cookies)")?;

        Ok(page.items.into_iter().map(map_album_item).collect())
    }

    /// Genera la cola de recomendaciones automáticas de Google ("Radio / A continuación")
    /// a partir de una canción (`RDAMVM<video_id>`).
    /// Si la cuenta está vinculada, las recomendaciones son personalizadas por Google.
    pub async fn get_radio_for_track(&self, video_id: &str) -> Result<RadioPage> {
        let page = self
            .music_query()
            .music_radio_track(video_id)
            .await
            .context("Error al obtener recomendaciones de Radio de Google")?;

        Ok(RadioPage {
            tracks: page.items.into_iter().map(map_track).collect(),
            continuation: page.ctoken,
            visitor_data: page.visitor_data,
        })
    }

    /// Genera la radio de Google a partir de una playlist/álbum (`RDAMPL<playlist_id>`).
    pub async fn get_radio_for_playlist(&self, playlist_id: &str) -> Result<RadioPage> {
        let page = self
            .music_query()
            .music_radio_playlist(playlist_id)
            .await
            .context("Error al obtener radio del álbum en YouTube Music")?;

        Ok(RadioPage {
            tracks: page.items.into_iter().map(map_track).collect(),
            continuation: page.ctoken,
            visitor_data: page.visitor_data,
        })
    }

    /// Carga la siguiente página de recomendaciones de Radio usando el token de continuación.
    pub async fn get_radio_continuation(
        &self,
        ctoken: &str,
        visitor_data: Option<&str>,
    ) -> Result<RadioPage> {
        let page = self
            .music_query()
            .continuation::<TrackItem, _>(ctoken, ContinuationEndpoint::MusicNext, visitor_data)
            .await
            .context("Error al cargar más recomendaciones de Radio")?;

        Ok(RadioPage {
            tracks: page.items.into_iter().map(map_track).collect(),
            continuation: page.ctoken,
            visitor_data: page.visitor_data,
        })
    }

    /// Resuelve la URL del stream de audio de mayor calidad compatible con decodificación nativa (M4A/AAC).
    ///
    /// Prioriza clientes de alta fiabilidad sin corte de 1 MB (primero `VISIONOS` nativo y luego respaldos
    /// de RustyPipe), verificando tanto el inicio del stream como un bloque posterior a 1 MB antes de reproducir.
    pub async fn resolve_audio_stream(&self, video_id: &str) -> Result<ResolvedAudio> {
        // 1) Cliente nativo VISIONOS: entrega streams AAC/M4A directos completos sin corte 403 al 1 MB (~50s).
        let visitor_data = self.rp.query().get_visitor_data(false).await.ok();
        if let Ok(resolved) = resolve_via_visionos(video_id, visitor_data.as_deref()).await {
            return Ok(resolved);
        }

        let status = self.auth_status();

        // 2) Respaldos de RustyPipe si VISIONOS no estuviera disponible para alguna pista específica.
        let mut attempts: Vec<(ClientType, bool)> = Vec::new();
        if status.has_oauth {
            attempts.push((ClientType::Tv, true));
        }
        attempts.push((ClientType::Ios, false));
        attempts.push((ClientType::Android, false));
        attempts.push((ClientType::Tv, false));
        if status.has_cookie {
            attempts.push((ClientType::Desktop, true));
        }
        attempts.push((ClientType::Desktop, false));

        let mut last_err = String::from("No se encontró un stream de audio reproducible");

        for (client_type, use_auth) in attempts {
            let query = if use_auth {
                self.rp.query().authenticated()
            } else {
                self.rp.query().unauthenticated()
            };

            let player = match query.player_from_client(video_id, client_type).await {
                Ok(p) => p,
                Err(e) => {
                    last_err = format!("{client_type:?}: {e}");
                    tracing::debug!("Player fallback ({client_type:?}): {e}");
                    continue;
                }
            };

            let Some(best) = select_best_compatible_audio(&player.audio_streams) else {
                last_err = format!("{client_type:?}: sin streams M4A/AAC compatibles");
                continue;
            };

            let ua = query.user_agent(player.client_type).into_owned();
            let content_len = (best.size > 0).then_some(best.size);
            if probe_stream_url(&best.url, &ua, content_len).await {
                let bitrate_kbps = best.bitrate / 1000;
                let duration_secs = best
                    .duration_ms
                    .map(|ms| ms / 1000)
                    .or((player.details.duration > 0).then_some(player.details.duration));

                return Ok(ResolvedAudio {
                    url: best.url.clone(),
                    user_agent: ua,
                    bitrate_kbps,
                    codec_label: format!("AAC {} kbps", bitrate_kbps),
                    loudness_db: best.loudness_db,
                    duration_secs,
                    content_length: content_len,
                });
            } else {
                last_err = format!("{client_type:?}: el servidor rechazó el stream (403/corte >1MB)");
            }
        }

        // 3) Respaldo opcional silencioso si yt-dlp está instalado en el sistema
        if let Ok(fallback) = resolve_via_ytdlp_fallback(video_id).await {
            return Ok(fallback);
        }

        Err(anyhow!(
            "No se pudo obtener un stream reproducible para {video_id}: {last_err}"
        ))
    }
}

/// Selecciona el stream de audio no-DRM compatible con Symphonia/Rodio de mayor bitrate.
fn select_best_compatible_audio(streams: &[AudioStream]) -> Option<&AudioStream> {
    let mut candidates: Vec<&AudioStream> = streams
        .iter()
        .filter(|s| s.drm_track_type.is_none())
        .filter(|s| s.codec == AudioCodec::Mp4a)
        .collect();

    if candidates.is_empty() {
        return None;
    }

    // Preferimos pista original/por defecto y, dentro de ellas, el mayor bitrate disponible
    // (p. ej. itag 141 256kbps AAC Premium > itag 140 128kbps AAC).
    candidates.sort_by_key(|s| {
        let track_priority: u8 = match &s.track {
            None => 3,
            Some(t) if t.track_type == Some(AudioTrackType::Original) => 3,
            Some(t) if t.is_default => 2,
            Some(_) => 1,
        };
        (track_priority, s.bitrate, s.average_bitrate)
    });

    candidates.pop()
}

/// Resuelve el stream de audio AAC/M4A completo vía cliente InnerTube `VISIONOS` (sin corte 403 al 1 MB).
async fn resolve_via_visionos(video_id: &str, visitor_data: Option<&str>) -> Result<ResolvedAudio> {
    const VISIONOS_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 15_7_3) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.0 Safari/605.1.15";
    const VISIONOS_VER: &str = "1.02";

    let client = sd_reqwest::Client::builder()
        .user_agent(VISIONOS_UA)
        .timeout(Duration::from_secs(12))
        .build()
        .context("No se pudo crear cliente HTTP para VISIONOS")?;

    let mut client_ctx = serde_json::json!({
        "clientName": "VISIONOS",
        "clientVersion": VISIONOS_VER,
        "deviceMake": "Apple",
        "deviceModel": "RealityDevice17,1",
        "osName": "visionOS",
        "osVersion": "26.5.23O471",
        "hl": "en",
        "gl": "US"
    });
    if let Some(vd) = visitor_data.filter(|s| !s.is_empty()) {
        client_ctx["visitorData"] = serde_json::Value::String(vd.to_string());
    }

    let payload = serde_json::json!({
        "context": { "client": client_ctx },
        "videoId": video_id,
        "contentCheckOk": true,
        "racyCheckOk": true
    });

    let mut req = client
        .post("https://www.youtube.com/youtubei/v1/player?prettyPrint=false")
        .header("X-YouTube-Client-Name", "101")
        .header("X-YouTube-Client-Version", VISIONOS_VER)
        .header(sd_reqwest::header::CONTENT_TYPE, "application/json")
        .body(payload.to_string());
    if let Some(vd) = visitor_data.filter(|s| !s.is_empty()) {
        req = req.header("X-Goog-Visitor-Id", vd);
    }

    let resp = req
        .send()
        .await
        .context("Error consultando player VISIONOS")?;
    if !resp.status().is_success() {
        return Err(anyhow!("VISIONOS HTTP {}", resp.status()));
    }

    let text = resp
        .text()
        .await
        .context("Error leyendo respuesta VISIONOS")?;
    let json: serde_json::Value =
        serde_json::from_str(&text).context("JSON inválido de VISIONOS")?;

    if json["playabilityStatus"]["status"].as_str() != Some("OK") {
        let reason = json["playabilityStatus"]["reason"]
            .as_str()
            .unwrap_or("UNPLAYABLE");
        return Err(anyhow!("VISIONOS no reproducible: {reason}"));
    }

    let formats = json["streamingData"]["adaptiveFormats"]
        .as_array()
        .ok_or_else(|| anyhow!("VISIONOS sin adaptiveFormats"))?;

    let best = formats
        .iter()
        .filter(|f| {
            let mime = f["mimeType"].as_str().unwrap_or("");
            let is_mp4_audio = mime.starts_with("audio/mp4") && mime.contains("mp4a");
            let has_url = f["url"].as_str().map(|u| !u.is_empty()).unwrap_or(false);
            let is_drm = f["drmFamilies"].is_array();
            is_mp4_audio && has_url && !is_drm
        })
        .max_by_key(|f| {
            let track_prio: u8 = match f.get("audioTrack") {
                None => 3,
                Some(t)
                    if t["displayName"]
                        .as_str()
                        .map(|d| d.to_ascii_lowercase().contains("original"))
                        .unwrap_or(false) =>
                {
                    3
                }
                Some(t) if t["audioIsDefault"].as_bool().unwrap_or(false) => 2,
                Some(_) => 1,
            };
            let br = f["bitrate"].as_u64().unwrap_or(0);
            let avg = f["averageBitrate"].as_u64().unwrap_or(0);
            (track_prio, br, avg)
        })
        .ok_or_else(|| anyhow!("VISIONOS sin stream AAC/M4A compatible"))?;

    let url = best["url"]
        .as_str()
        .ok_or_else(|| anyhow!("VISIONOS stream sin URL"))?
        .to_string();
    let content_length = best["contentLength"]
        .as_str()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&n| n > 0);

    if !probe_stream_url(&url, VISIONOS_UA, content_length).await {
        return Err(anyhow!("VISIONOS stream rechazado en verificación Range (>1MB)"));
    }

    let bitrate_bps = best["bitrate"].as_u64().unwrap_or(128_000) as u32;
    let bitrate_kbps = bitrate_bps / 1000;
    let duration_secs = best["approxDurationMs"]
        .as_str()
        .and_then(|s| s.parse::<u64>().ok())
        .map(|ms| (ms / 1000) as u32)
        .or_else(|| {
            json["videoDetails"]["lengthSeconds"]
                .as_str()
                .and_then(|s| s.parse::<u32>().ok())
        });
    let loudness_db = best["loudnessDb"]
        .as_f64()
        .or_else(|| json["playerConfig"]["audioConfig"]["loudnessDb"].as_f64())
        .map(|v| v as f32);

    Ok(ResolvedAudio {
        url,
        user_agent: VISIONOS_UA.to_string(),
        bitrate_kbps,
        codec_label: format!("AAC {} kbps", bitrate_kbps),
        loudness_db,
        duration_secs,
        content_length,
    })
}

/// Comprueba con peticiones `Range` que Google entrega audio real tanto al inicio (`0..64KB`)
/// como más allá del umbral de 1 MiB (`>1.05MB`) para descartar streams que se cortan con 403 a los ~50s.
async fn probe_stream_url(url: &str, user_agent: &str, content_length: Option<u64>) -> bool {
    let Ok(client) = sd_reqwest::Client::builder()
        .user_agent(user_agent)
        .timeout(Duration::from_secs(8))
        .build()
    else {
        return false;
    };

    async fn check_range(client: &sd_reqwest::Client, url: &str, range: &str) -> bool {
        match client
            .get(url)
            .header(sd_reqwest::header::RANGE, range)
            .send()
            .await
        {
            Ok(mut resp) => {
                let status = resp.status();
                if !status.is_success() && status != sd_reqwest::StatusCode::PARTIAL_CONTENT {
                    return false;
                }
                matches!(resp.chunk().await, Ok(Some(bytes)) if !bytes.is_empty())
            }
            Err(_) => false,
        }
    }

    if !check_range(&client, url, "bytes=0-65535").await {
        return false;
    }

    if let Some(total) = content_length
        && total > 1_100_000
    {
        let start = 1_100_000u64;
        let end = (start + 65_535).min(total - 1);
        let late_range = format!("bytes={start}-{end}");
        if !check_range(&client, url, &late_range).await {
            return false;
        }
    }

    true
}

/// Respaldo opcional si el usuario tiene `yt-dlp` instalado y los clientes nativos fallan.
async fn resolve_via_ytdlp_fallback(video_id: &str) -> Result<ResolvedAudio> {
    let url = format!("https://music.youtube.com/watch?v={video_id}");
    let out = tokio::process::Command::new("yt-dlp")
        .args(["--no-playlist", "-f", "ba[ext=m4a]/ba", "-g", &url])
        .output()
        .await?;

    if !out.status.success() {
        return Err(anyhow!("yt-dlp fallback falló"));
    }
    let stream_url = String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    if stream_url.is_empty() {
        return Err(anyhow!("yt-dlp no devolvió URL"));
    }

    Ok(ResolvedAudio {
        url: stream_url,
        user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36".to_string(),
        bitrate_kbps: 128,
        codec_label: "M4A (yt-dlp)".to_string(),
        loudness_db: None,
        duration_secs: None,
        content_length: None,
    })
}

fn best_thumbnail(thumbs: &[Thumbnail]) -> Option<String> {
    thumbs
        .iter()
        .max_by_key(|t| t.width.saturating_mul(t.height))
        .map(|t| t.url.clone())
}

fn join_artists(artists: &[rustypipe::model::ArtistId], by_va: bool) -> String {
    if artists.is_empty() {
        if by_va {
            "Varios artistas".to_string()
        } else {
            "Artista desconocido".to_string()
        }
    } else {
        artists
            .iter()
            .map(|a| a.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn map_track(t: TrackItem) -> Track {
    Track {
        id: t.id,
        title: t.name,
        artist: join_artists(&t.artists, t.by_va),
        album: t.album.map(|a| a.name),
        duration_secs: t.duration,
        cover_url: best_thumbnail(&t.cover),
    }
}

fn album_type_label(t: AlbumType) -> String {
    match t {
        AlbumType::Album => "Álbum".to_string(),
        AlbumType::Ep => "EP".to_string(),
        AlbumType::Single => "Sencillo".to_string(),
        AlbumType::Audiobook => "Audiolibro".to_string(),
        AlbumType::Show => "Programa".to_string(),
        _ => "Álbum".to_string(),
    }
}

fn map_album_item(a: AlbumItem) -> AlbumSummary {
    AlbumSummary {
        id: a.id,
        title: a.name,
        artist: join_artists(&a.artists, a.by_va),
        year: a.year,
        album_type: album_type_label(a.album_type),
        cover_url: best_thumbnail(&a.cover),
    }
}

fn map_album_detail(a: MusicAlbum) -> AlbumDetail {
    let cover_url = best_thumbnail(&a.cover);
    let summary = AlbumSummary {
        id: a.id,
        title: a.name,
        artist: join_artists(&a.artists, a.by_va),
        year: a.year,
        album_type: album_type_label(a.album_type),
        cover_url: cover_url.clone(),
    };

    let tracks = a
        .tracks
        .into_iter()
        .map(|t| {
            let mut mapped = map_track(t);
            if mapped.cover_url.is_none() {
                mapped.cover_url = cover_url.clone();
            }
            if mapped.album.is_none() {
                mapped.album = Some(summary.title.clone());
            }
            mapped
        })
        .collect();

    AlbumDetail {
        summary,
        tracks,
        playlist_id: a.playlist_id,
    }
}
