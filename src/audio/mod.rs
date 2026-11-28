pub mod chunked_stream;

use std::io::BufReader;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use rodio::cpal::traits::{DeviceTrait, HostTrait};
use rodio::{
    CpalError, CpalErrorKind, Decoder, DeviceSinkBuilder, MixerDeviceSink, Player, Source,
};
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
        seek_to: Option<Duration>,
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

fn open_output_sink(sink_invalidated: Arc<AtomicBool>) -> Result<MixerDeviceSink> {
    let make_cb = || {
        let flag = sink_invalidated.clone();
        move |err: CpalError| {
            tracing::debug!("Evento del stream de audio (CPAL): {err}");
            if matches!(
                err.kind(),
                CpalErrorKind::StreamInvalidated | CpalErrorKind::DeviceNotAvailable
            ) {
                flag.store(true, Ordering::Release);
            }
        }
    };

    let primary = DeviceSinkBuilder::from_default_device()
        .and_then(|b| b.with_error_callback(make_cb()).open_sink_or_fallback());

    let mut sink = match primary {
        Ok(s) => s,
        Err(original_err) => {
            let devices = rodio::cpal::default_host()
                .output_devices()
                .map_err(|_| anyhow!("No se pudo abrir el dispositivo de salida de audio: {original_err}"))?;

            devices
                .filter(|dev| {
                    dev.description()
                        .map(|desc| desc.driver().is_some_and(|driver| driver != "null"))
                        .unwrap_or(false)
                })
                .find_map(|d| {
                    DeviceSinkBuilder::from_device(d)
                        .and_then(|b| b.with_error_callback(make_cb()).open_sink_or_fallback())
                        .ok()
                })
                .ok_or_else(|| anyhow!("No se pudo abrir el dispositivo de salida de audio: {original_err}"))?
        }
    };

    sink.log_on_drop(false);
    Ok(sink)
}

fn rebuild_sink_if_invalidated(
    sink_invalidated: &Arc<AtomicBool>,
    device_sink: &mut MixerDeviceSink,
    player: &mut Player,
    user_volume: f32,
    loudness_db: Option<f32>,
) {
    if sink_invalidated.swap(false, Ordering::AcqRel) {
        match open_output_sink(sink_invalidated.clone()) {
            Ok(new_sink) => {
                player.stop();
                *device_sink = new_sink;
                *player = Player::connect_new(device_sink.mixer());
                player.set_volume(effective_volume(user_volume, loudness_db));
            }
            Err(e) => {
                tracing::debug!("No se pudo reconstruir el dispositivo de audio: {e}");
                sink_invalidated.store(true, Ordering::Release);
            }
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
    init_tx: mpsc::SyncSender<Result<()>>,
) {
    let sink_invalidated = Arc::new(AtomicBool::new(false));
    let mut device_sink = match open_output_sink(sink_invalidated.clone()) {
        Ok(sink) => {
            let _ = init_tx.send(Ok(()));
            sink
        }
        Err(e) => {
            let _ = init_tx.send(Err(e));
            return;
        }
    };

    let mut player = Player::connect_new(device_sink.mixer());
    let mut user_volume = initial_volume.clamp(0.0, 1.0);
    let mut current_loudness_db: Option<f32> = None;
    let mut active_track_id: Option<String> = None;
    let mut current_stream: Option<ResolvedAudio> = None;
    let mut playback_started = false;
    let mut last_pos_sample = Duration::ZERO;
    let mut stalled_ticks: u8 = 0;
    let mut recovered_for_gen: Option<u64> = None;

    player.set_volume(effective_volume(user_volume, current_loudness_db));

    loop {
        match cmd_rx.recv_timeout(Duration::from_millis(120)) {
            Ok(AudioCommand::Load {
                generation: req_gen,
                track_id,
                stream,
            }) => {
                rebuild_sink_if_invalidated(
                    &sink_invalidated,
                    &mut device_sink,
                    &mut player,
                    user_volume,
                    stream.loudness_db,
                );
                // Detenemos inmediatamente la pista anterior
                player.stop();
                player = Player::connect_new(device_sink.mixer());
                player.set_volume(effective_volume(user_volume, stream.loudness_db));
                playback_started = false;
                active_track_id = Some(track_id.clone());
                current_stream = Some(stream.clone());
                last_pos_sample = Duration::ZERO;
                stalled_ticks = 0;
                recovered_for_gen = None;

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
                                seek_to: None,
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
                seek_to,
            }) => {
                if generation.load(Ordering::SeqCst) != req_gen {
                    continue;
                }
                current_loudness_db = loudness_db;
                player.stop();
                player = Player::connect_new(device_sink.mixer());
                player.set_volume(effective_volume(user_volume, current_loudness_db));
                player.append(source);
                let mut initial_pos = Duration::ZERO;
                if let Some(seek_pos) = seek_to
                    && seek_pos > Duration::ZERO
                {
                    let _ = player.try_seek(seek_pos);
                    initial_pos = seek_pos;
                }
                player.play();
                active_track_id = Some(track_id.clone());
                playback_started = true;
                last_pos_sample = initial_pos;
                stalled_ticks = 0;

                let mut snap = snapshot.lock().unwrap();
                snap.status = PlaybackStatus::Playing;
                snap.track_id = Some(track_id);
                snap.position = initial_pos;
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
                current_stream = None;
                last_pos_sample = Duration::ZERO;
                stalled_ticks = 0;
                recovered_for_gen = None;
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
                        last_pos_sample = pos;
                        stalled_ticks = 0;
                        snapshot.lock().unwrap().position = pos;
                    }
                }
            }
            Ok(AudioCommand::SetVolume(vol)) => {
                user_volume = vol.clamp(0.0, 1.0);
                player.set_volume(effective_volume(user_volume, current_loudness_db));
            }
            Ok(AudioCommand::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                player.stop();
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if !playback_started && sink_invalidated.load(Ordering::Acquire) {
                    rebuild_sink_if_invalidated(
                        &sink_invalidated,
                        &mut device_sink,
                        &mut player,
                        user_volume,
                        current_loudness_db,
                    );
                }
            }
        }

        // Actualizamos progreso o detectamos fin natural de pista
        if playback_started {
            if player.empty() {
                playback_started = false;
                current_stream = None;
                last_pos_sample = Duration::ZERO;
                stalled_ticks = 0;
                let finished_id = active_track_id.take();
                let mut snap = snapshot.lock().unwrap();
                snap.status = PlaybackStatus::Idle;
                snap.ended_track_id = finished_id;
            } else {
                let pos = player.get_pos();
                let paused = player.is_paused();
                {
                    let mut snap = snapshot.lock().unwrap();
                    snap.position = pos;
                    snap.status = if paused {
                        PlaybackStatus::Paused
                    } else {
                        PlaybackStatus::Playing
                    };
                }

                if !paused && sink_invalidated.load(Ordering::Acquire) {
                    if pos == last_pos_sample {
                        stalled_ticks = stalled_ticks.saturating_add(1);
                    } else {
                        last_pos_sample = pos;
                        stalled_ticks = 0;
                    }

                    if stalled_ticks >= 10
                        && recovered_for_gen != Some(generation.load(Ordering::SeqCst))
                        && let (Some(tid), Some(stream)) =
                            (active_track_id.clone(), current_stream.clone())
                    {
                        match open_output_sink(sink_invalidated.clone()) {
                            Ok(new_sink) => {
                                sink_invalidated.store(false, Ordering::Release);
                                stalled_ticks = 0;
                                player.stop();
                                device_sink = new_sink;
                                player = Player::connect_new(device_sink.mixer());
                                player.set_volume(effective_volume(
                                    user_volume,
                                    stream.loudness_db,
                                ));
                                playback_started = false;

                                let req_gen = generation.fetch_add(1, Ordering::SeqCst) + 1;
                                recovered_for_gen = Some(req_gen);

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
                                            let duration = decoded_dur.or_else(|| {
                                                stream
                                                    .duration_secs
                                                    .map(|s| Duration::from_secs(s.into()))
                                            });
                                            let _ = tx.send(AudioCommand::Prepared {
                                                generation: req_gen,
                                                track_id: tid,
                                                source,
                                                duration,
                                                codec_label: stream.codec_label,
                                                loudness_db: stream.loudness_db,
                                                seek_to: Some(pos),
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
                            Err(e) => {
                                tracing::debug!(
                                    "Reintento de dispositivo de audio tras bloqueo falló: {e}"
                                );
                                stalled_ticks = 0;
                            }
                        }
                    }
                } else {
                    last_pos_sample = pos;
                    stalled_ticks = 0;
                }
            }
        }
    }
}

/// Normalización de volumen estilo YouTube Music (solo atenúa pistas muy fuertes para evitar clipping).
fn effective_volume(user_volume: f32, loudness_db: Option<f32>) -> f32 {
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
