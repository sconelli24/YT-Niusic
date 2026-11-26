use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use souvlaki::{MediaControlEvent, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig};
use tokio::sync::mpsc as tokio_mpsc;

use crate::ytm::Track;

/// Eventos simplificados recibidos desde las teclas multimedia del sistema operativo
/// (teclado físico, auriculares Bluetooth, overlay multimedia de Windows 10/11).
#[derive(Debug, Clone)]
pub enum OsMediaEvent {
    Play,
    Pause,
    Toggle,
    Next,
    Previous,
    Stop,
    SeekTo(Duration),
    SeekBySecs(i64),
}

enum MediaCmd {
    SetMetadata {
        title: String,
        artist: String,
        album: Option<String>,
        cover_url: Option<String>,
        duration: Option<Duration>,
    },
    SetPlayback {
        playing: bool,
        stopped: bool,
        position: Option<Duration>,
    },
    Shutdown,
}

/// Controlador de teclas multimedia del SO (SMTC en Windows).
/// Corre en un hilo liviano dedicado y falla de forma segura (no-op) si el SO no soporta SMTC.
pub struct MediaKeysHandle {
    tx: Option<mpsc::Sender<MediaCmd>>,
}

impl MediaKeysHandle {
    pub fn spawn(event_tx: tokio_mpsc::UnboundedSender<OsMediaEvent>) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel::<MediaCmd>();

        let spawned = thread::Builder::new()
            .name("yt-niusic-smtc".into())
            .spawn(move || {
                run_media_keys_thread(cmd_rx, event_tx);
            });

        match spawned {
            Ok(_) => Self { tx: Some(cmd_tx) },
            Err(_) => Self { tx: None },
        }
    }

    pub fn update_metadata(&self, track: &Track, duration: Option<Duration>) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(MediaCmd::SetMetadata {
                title: track.title.clone(),
                artist: track.artist.clone(),
                album: track.album.clone(),
                cover_url: track.cover_url.clone(),
                duration: duration
                    .or_else(|| track.duration_secs.map(|s| Duration::from_secs(s.into()))),
            });
        }
    }

    pub fn update_playback(&self, playing: bool, stopped: bool, position: Option<Duration>) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(MediaCmd::SetPlayback {
                playing,
                stopped,
                position,
            });
        }
    }
}

impl Drop for MediaKeysHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(MediaCmd::Shutdown);
        }
    }
}

#[cfg(windows)]
fn run_media_keys_thread(
    cmd_rx: mpsc::Receiver<MediaCmd>,
    event_tx: tokio_mpsc::UnboundedSender<OsMediaEvent>,
) {
    use std::ffi::c_void;
    use std::ptr;
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::System::Console::GetConsoleWindow;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, DispatchMessageW, PeekMessageW, TranslateMessage, MSG,
        PM_REMOVE,
    };

    // Creamos una ventana oculta propia (0x0) para que Windows SMTC funcione de forma fiable
    // incluso dentro de Windows Terminal / ConPTY (donde GetConsoleWindow pertenece a conhost).
    let class_name: Vec<u16> = "STATIC\0".encode_utf16().collect();
    let window_title: Vec<u16> = "YT-Niusic\0".encode_utf16().collect();

    let (hwnd, owned_window): (HWND, bool) = unsafe {
        let created = CreateWindowExW(
            0,
            class_name.as_ptr(),
            window_title.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null(),
        );
        if !created.is_null() {
            (created, true)
        } else {
            (GetConsoleWindow(), false)
        }
    };

    if hwnd.is_null() {
        return;
    }

    let config = PlatformConfig {
        dbus_name: "yt_niusic",
        display_name: "YT-Niusic",
        hwnd: Some(hwnd as *mut c_void),
    };

    let mut controls = match souvlaki::MediaControls::new(config) {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!("SMTC no disponible: {e}");
            if owned_window {
                unsafe {
                    DestroyWindow(hwnd);
                }
            }
            return;
        }
    };

    let attach_res = controls.attach(move |ev: MediaControlEvent| {
        let mapped = match ev {
            MediaControlEvent::Play => Some(OsMediaEvent::Play),
            MediaControlEvent::Pause => Some(OsMediaEvent::Pause),
            MediaControlEvent::Toggle => Some(OsMediaEvent::Toggle),
            MediaControlEvent::Next => Some(OsMediaEvent::Next),
            MediaControlEvent::Previous => Some(OsMediaEvent::Previous),
            MediaControlEvent::Stop => Some(OsMediaEvent::Stop),
            MediaControlEvent::SetPosition(pos) => Some(OsMediaEvent::SeekTo(pos.0)),
            MediaControlEvent::Seek(dir) => Some(OsMediaEvent::SeekBySecs(match dir {
                souvlaki::SeekDirection::Forward => 10,
                souvlaki::SeekDirection::Backward => -10,
            })),
            MediaControlEvent::SeekBy(dir, dur) => {
                let s = dur.as_secs() as i64;
                Some(OsMediaEvent::SeekBySecs(match dir {
                    souvlaki::SeekDirection::Forward => s,
                    souvlaki::SeekDirection::Backward => -s,
                }))
            }
            _ => None,
        };
        if let Some(m) = mapped {
            let _ = event_tx.send(m);
        }
    });

    if let Err(e) = attach_res {
        tracing::debug!("No se pudo adjuntar SMTC: {e}");
        if owned_window {
            unsafe {
                DestroyWindow(hwnd);
            }
        }
        return;
    }

    let _ = controls.set_playback(MediaPlayback::Stopped);

    loop {
        // Procesamos mensajes Win32 pendientes de nuestra ventana oculta
        unsafe {
            let mut msg: MSG = std::mem::zeroed();
            while PeekMessageW(&mut msg, hwnd, 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        match cmd_rx.recv_timeout(Duration::from_millis(80)) {
            Ok(MediaCmd::SetMetadata {
                title,
                artist,
                album,
                cover_url,
                duration,
            }) => {
                let _ = controls.set_metadata(MediaMetadata {
                    title: Some(title.as_str()),
                    artist: Some(artist.as_str()),
                    album: album.as_deref(),
                    cover_url: cover_url.as_deref(),
                    duration,
                });
            }
            Ok(MediaCmd::SetPlayback {
                playing,
                stopped,
                position,
            }) => {
                let pb = if stopped {
                    MediaPlayback::Stopped
                } else if playing {
                    MediaPlayback::Playing {
                        progress: position.map(MediaPosition),
                    }
                } else {
                    MediaPlayback::Paused {
                        progress: position.map(MediaPosition),
                    }
                };
                let _ = controls.set_playback(pb);
            }
            Ok(MediaCmd::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }

    drop(controls);
    if owned_window {
        unsafe {
            DestroyWindow(hwnd);
        }
    }
}

#[cfg(not(windows))]
fn run_media_keys_thread(
    cmd_rx: mpsc::Receiver<MediaCmd>,
    _event_tx: tokio_mpsc::UnboundedSender<OsMediaEvent>,
) {
    while let Ok(cmd) = cmd_rx.recv() {
        if matches!(cmd, MediaCmd::Shutdown) {
            break;
        }
    }
}
