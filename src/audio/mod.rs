pub mod chunked_stream;

use std::io::BufReader;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use rodio::{Decoder, DeviceSinkBuilder, Player, Source};
use stream_download::source::DecodeError;
use stream_download::storage::temp::TempStorageProvider;
use stream_download::{Settings, StreamDownload};

use crate::audio::chunked_stream::{ChunkedYtParams, ChunkedYtStream};
use crate::ytm::ResolvedAudio;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaybackStatus {
    Idle,
    Buffering,
    Playing,
    Paused,
    Error(String),
}

#[derive(Debug, Clone)]
pub struct PlaybackSnapshot {
    pub status: PlaybackStatus,
    pub track_id: Option<String>,
    pub position: Duration,
    pub duration: Option<Duration>,
    pub codec_label: Option<String>,
    /// Se marca con el ID de la canción cuando la pista actual terminó naturalmente.
    pub ended_track_id: Option<String>,
}

impl Default for PlaybackSnapshot {
    fn default() -> Self {
        Self {
            status: PlaybackStatus::Idle,
            track_id: None,
            position: Duration::ZERO,
            duration: None,
            codec_label: None,
            ended_track_id: None,
        }
    }
}

type DecodedStream = Decoder<BufReader<StreamDownload<TempStorageProvider>>>;

enum AudioCommand {
    Load {
        generation: u64,
        track_id: String,
        stream: ResolvedAudio,
    },
    Prepared {
        generation: u64,
        track_id: String,
        source: DecodedStream,
        duration: Option<Duration>,
        codec_label: String,
        loudness_db: Option<f32>,
    },
    LoadFailed {
        generation: u64,
        error: String,
    },
    Pause,
    Resume,
    TogglePause,
    Stop,
    Seek(Duration),
    SetVolume(f32),
    SetNormalizeLoudness(bool),
    Shutdown,
}

/// Motor de audio asíncrono en hilo dedicado con streaming HTTP progresivo y seek.
#[derive(Clone)]
pub struct AudioEngine {
    cmd_tx: mpsc::Sender<AudioCommand>,
    snapshot: Arc<Mutex<PlaybackSnapshot>>,
    generation: Arc<AtomicU64>,
    runtime: tokio::runtime::Handle,
}

impl AudioEngine {
    pub fn spawn(
        runtime: tokio::runtime::Handle,
        initial_volume: f32,
        normalize_loudness: bool,
    ) -> Result<Self> {
        let (cmd_tx, cmd_rx) = mpsc::channel::<AudioCommand>();
        let snapshot = Arc::new(Mutex::new(PlaybackSnapshot::default()));
        let generation = Arc::new(AtomicU64::new(0));

        let (init_tx, init_rx) = mpsc::sync_channel::<Result<()>>(1);
        let snap_clone = snapshot.clone();
        let gen_clone = generation.clone();
        let tx_clone = cmd_tx.clone();
        let rt_clone = runtime.clone();

        thread::Builder::new()
            .name("yt-niusic-audio".into())
            .spawn(move || {
                run_audio_thread(
                    cmd_rx,
                    tx_clone,
                    snap_clone,
                    gen_clone,
                    rt_clone,
                    initial_volume,
                    normalize_loudness,
                    init_tx,
                );
            })
            .context("No se pudo iniciar el hilo de audio")?;

        init_rx
            .recv()
            .context("El hilo de audio terminó inesperadamente al iniciar")??;

        Ok(Self {
            cmd_tx,
            snapshot,
            generation,
            runtime,
        })
    }

    /// Inicia la carga y reproducción de un stream resuelto, cancelando cualquier pista anterior.
    pub fn play_stream(&self, track_id: String, stream: ResolvedAudio) {
        let req_gen = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        {
            let mut snap = self.snapshot.lock().unwrap();
            snap.status = PlaybackStatus::Buffering;
            snap.track_id = Some(track_id.clone());
            snap.position = Duration::ZERO;
            snap.duration = stream.duration_secs.map(|s| Duration::from_secs(s.into()));
            snap.codec_label = Some(stream.codec_label.clone());
            snap.ended_track_id = None;
        }
        let _ = self.cmd_tx.send(AudioCommand::Load {
            generation: req_gen,
            track_id,
            stream,
        });
    }

    pub fn pause(&self) {
        let _ = self.cmd_tx.send(AudioCommand::Pause);
    }

    pub fn resume(&self) {
        let _ = self.cmd_tx.send(AudioCommand::Resume);
    }

    pub fn toggle_pause(&self) {
        let _ = self.cmd_tx.send(AudioCommand::TogglePause);
    }

    pub fn stop(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        let _ = self.cmd_tx.send(AudioCommand::Stop);
    }

    pub fn seek(&self, position: Duration) {
        let _ = self.cmd_tx.send(AudioCommand::Seek(position));
    }

    pub fn set_volume(&self, volume: f32) {
        let _ = self.cmd_tx.send(AudioCommand::SetVolume(volume.clamp(0.0, 1.0)));
    }

    pub fn set_normalize_loudness(&self, enabled: bool) {
        let _ = self.cmd_tx.send(AudioCommand::SetNormalizeLoudness(enabled));
    }

    /// Lee el estado actual y consume el flag `ended_track_id` si la canción terminó.
    pub fn poll_snapshot(&self) -> PlaybackSnapshot {
        let mut snap = self.snapshot.lock().unwrap();
        let out = snap.clone();
        snap.ended_track_id = None;
        out
    }

    pub fn runtime(&self) -> &tokio::runtime::Handle {
        &self.runtime
    }
}

impl Drop for AudioEngine {
    fn drop(&mut self) {
        // Solo cerramos si somos la última referencia fuerte al snapshot (evita cerrar al clonar)
        if Arc::strong_count(&self.snapshot) == 2 {
            let _ = self.cmd_tx.send(AudioCommand::Shutdown);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_audio_thread(
    cmd_rx: mpsc::Receiver<AudioCommand>,
    cmd_tx: mpsc::Sender<AudioCommand>,
    snapshot: Arc<Mutex<PlaybackSnapshot>>,
    generation: Arc<AtomicU64>,
    runtime: tokio::runtime::Handle,
    initial_volume: f32,
    initial_normalize: bool,
    init_tx: mpsc::SyncSender<Result<()>>,
) {
    let device_sink = match DeviceSinkBuilder::open_default_sink() {
        Ok(sink) => {
            let _ = init_tx.send(Ok(()));
            sink
        }
        Err(e) => {
            let _ = init_tx.send(Err(anyhow!(
                "No se pudo abrir el dispositivo de salida de audio: {e}"
            )));
            return;
        }
    };

    let mut player = Player::connect_new(device_sink.mixer());
    let mut user_volume = initial_volume.clamp(0.0, 1.0);
    let mut normalize_loudness = initial_normalize;
    let mut current_loudness_db: Option<f32> = None;
    let mut active_track_id: Option<String> = None;
    let mut playback_started = false;

    player.set_volume(effective_volume(
        user_volume,
        current_loudness_db,
        normalize_loudness,
    ));

    loop {
        match cmd_rx.recv_timeout(Duration::from_millis(120)) {
            Ok(AudioCommand::Load {
                generation: req_gen,
                track_id,
                stream,
            }) => {
                // Detenemos inmediatamente la pista anterior
                player.stop();
                player = Player::connect_new(device_sink.mixer());
                player.set_volume(effective_volume(user_volume, stream.loudness_db, normalize_loudness));
                playback_started = false;
                active_track_id = Some(track_id.clone());

                let tx = cmd_tx.clone();
                let gen_ref = generation.clone();
                let rt = runtime.clone();

                rt.spawn(async move {
                    if gen_ref.load(Ordering::SeqCst) != req_gen {
                        return;
                    }
                    match prepare_decoded_stream(&stream).await {
                        Ok((source, decoded_dur)) => {
                            if gen_ref.load(Ordering::SeqCst) != req_gen {
                                return;
                            }
                            let duration = decoded_dur
                                .or_else(|| stream.duration_secs.map(|s| Duration::from_secs(s.into())));
                            let _ = tx.send(AudioCommand::Prepared {
                                generation: req_gen,
                                track_id,
                                source,
                                duration,
                                codec_label: stream.codec_label,
                                loudness_db: stream.loudness_db,
                            });
                        }
                        Err(e) => {
                            if gen_ref.load(Ordering::SeqCst) != req_gen {
                                return;
                            }
                            let _ = tx.send(AudioCommand::LoadFailed {
                                generation: req_gen,
                                error: e.to_string(),
                            });
                        }
                    }
                });
            }
            Ok(AudioCommand::Prepared {
                generation: req_gen,
                track_id,
                source,
                duration,
                codec_label,
                loudness_db,
            }) => {
                if generation.load(Ordering::SeqCst) != req_gen {
                    continue;
                }
                current_loudness_db = loudness_db;
                player.stop();
                player = Player::connect_new(device_sink.mixer());
                player.set_volume(effective_volume(
                    user_volume,
                    current_loudness_db,
                    normalize_loudness,
                ));
                player.append(source);
                player.play();
                active_track_id = Some(track_id.clone());
                playback_started = true;

                let mut snap = snapshot.lock().unwrap();
                snap.status = PlaybackStatus::Playing;
                snap.track_id = Some(track_id);
                snap.position = Duration::ZERO;
                snap.duration = duration;
                snap.codec_label = Some(codec_label);
                snap.ended_track_id = None;
            }
            Ok(AudioCommand::LoadFailed {
                generation: req_gen,
                error,
            }) => {
                if generation.load(Ordering::SeqCst) != req_gen {
                    continue;
                }
                playback_started = false;
                let mut snap = snapshot.lock().unwrap();
                snap.status = PlaybackStatus::Error(error);
            }
            Ok(AudioCommand::Pause) => {
                if playback_started {
                    player.pause();
                    let mut snap = snapshot.lock().unwrap();
                    snap.status = PlaybackStatus::Paused;
                }
            }
            Ok(AudioCommand::Resume) => {
                if playback_started {
                    player.play();
                    let mut snap = snapshot.lock().unwrap();
                    snap.status = PlaybackStatus::Playing;
                }
            }
            Ok(AudioCommand::TogglePause) => {
                if playback_started {
                    if player.is_paused() {
                        player.play();
                        snapshot.lock().unwrap().status = PlaybackStatus::Playing;
                    } else {
                        player.pause();
                        snapshot.lock().unwrap().status = PlaybackStatus::Paused;
                    }
                }
            }
            Ok(AudioCommand::Stop) => {
                player.stop();
                playback_started = false;
                active_track_id = None;
                let mut snap = snapshot.lock().unwrap();
                snap.status = PlaybackStatus::Idle;
                snap.position = Duration::ZERO;
                snap.ended_track_id = None;
            }
            Ok(AudioCommand::Seek(pos)) => {
                if playback_started {
                    if let Err(e) = player.try_seek(pos) {
                        tracing::debug!("Seek no soportado o fuera de rango: {e:?}");
                    } else {
                        snapshot.lock().unwrap().position = pos;
                    }
                }
            }
            Ok(AudioCommand::SetVolume(vol)) => {
                user_volume = vol.clamp(0.0, 1.0);
                player.set_volume(effective_volume(
                    user_volume,
                    current_loudness_db,
                    normalize_loudness,
                ));
            }
            Ok(AudioCommand::SetNormalizeLoudness(enabled)) => {
                normalize_loudness = enabled;
                player.set_volume(effective_volume(
                    user_volume,
                    current_loudness_db,
                    normalize_loudness,
                ));
            }
            Ok(AudioCommand::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                player.stop();
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }

        // Actualizamos progreso o detectamos fin natural de pista
        if playback_started {
            if player.empty() {
                playback_started = false;
                let finished_id = active_track_id.take();
                let mut snap = snapshot.lock().unwrap();
                snap.status = PlaybackStatus::Idle;
                snap.ended_track_id = finished_id;
            } else {
                let pos = player.get_pos();
                let paused = player.is_paused();
                let mut snap = snapshot.lock().unwrap();
                snap.position = pos;
                snap.status = if paused {
                    PlaybackStatus::Paused
                } else {
                    PlaybackStatus::Playing
                };
            }
        }
    }
}

/// Normalización de volumen estilo YouTube Music (solo atenúa pistas muy fuertes para evitar clipping).
fn effective_volume(user_volume: f32, loudness_db: Option<f32>, normalize: bool) -> f32 {
    if !normalize {
        return user_volume;
    }
    match loudness_db {
        Some(db) if db > 0.0 => {
            let gain = 10f32.powf(-db / 20.0).clamp(0.25, 1.0);
            user_volume * gain
        }
        _ => user_volume,
    }
}

pub async fn prepare_decoded_stream(
    stream: &ResolvedAudio,
) -> Result<(DecodedStream, Option<Duration>)> {
    let params = ChunkedYtParams {
        url: stream.url.clone(),
        user_agent: stream.user_agent.clone(),
        content_length: stream.content_length,
    };

    let reader = match StreamDownload::new::<ChunkedYtStream>(
        params,
        TempStorageProvider::default(),
        Settings::default(),
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            let msg = e.decode_error().await;
            return Err(anyhow!("Error iniciando descarga progresiva: {msg}"));
        }
    };

    let decoded = tokio::task::spawn_blocking(move || -> Result<(DecodedStream, Option<Duration>)> {
        let buf_reader = BufReader::new(reader);
        let decoder = Decoder::try_from(buf_reader)
            .map_err(|e| anyhow!("Error decodificando audio (Symphonia/Rodio): {e}"))?;
        let total = decoder.total_duration();
        Ok((decoder, total))
    })
    .await
    .context("Falló la tarea de decodificación de audio")??;

    Ok(decoded)
}
