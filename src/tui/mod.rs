use std::collections::HashMap;
use std::io;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use tokio::sync::{mpsc, oneshot};
use zeroize::{Zeroize, Zeroizing};

use crate::audio::{AudioEngine, PlaybackStatus};
use crate::browser_auth::{detect_chromium_browser, open_url_in_default_browser};
use crate::config::{AppPaths, AppSettings};
use crate::media_keys::{MediaKeysHandle, OsMediaEvent};
use crate::queue::{PlaybackQueue, RadioFetchRequest, RadioSeed};
use crate::security::{cleanup_stale_auth_profiles, ENCRYPTED_SESSION_FILE};
use crate::updater::{
    check_for_updates, cleanup_previous_update_artifacts, download_and_install_update, UpdateInfo,
    CURRENT_VERSION,
};
use crate::ytm::{
    AlbumDetail, AlbumSummary, AuthStatus, LikeStatus, OAuthSession, RadioPage, ResolvedAudio,
    Track, YtmClient,
};

// === Paleta Amber Monochrome Console (24-bit RGB) ===
const BG_OBSIDIAN: Color = Color::Rgb(17, 13, 8);
const BG_PANEL: Color = Color::Rgb(22, 16, 10);
const AMBER_BRIGHT: Color = Color::Rgb(255, 212, 120);
const AMBER_MAIN: Color = Color::Rgb(255, 176, 46);
const AMBER_DIM: Color = Color::Rgb(176, 124, 48);
const BRONZE: Color = Color::Rgb(110, 75, 26);
const AMBER_ALERT: Color = Color::Rgb(255, 96, 56);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Screen {
    Login,
    Main,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Songs,
    Albums,
    Library,
    Auth,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FocusPane {
    Catalog,
    Queue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputMode {
    Normal,
    Search,
    PasteCookie,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LibrarySection {
    History,
    Liked,
    SavedAlbums,
}

enum AsyncEvent {
    SongsLoaded(Result<Vec<Track>, String>),
    AlbumsLoaded(Result<Vec<AlbumSummary>, String>),
    AlbumDetailLoaded(Result<AlbumDetail, String>),
    LibraryTracksLoaded(LibrarySection, Result<Vec<Track>, String>),
    LibraryAlbumsLoaded(Result<Vec<AlbumSummary>, String>),
    StreamResolved {
        request_id: u64,
        track_id: String,
        result: Result<ResolvedAudio, String>,
    },
    PrefetchResolved {
        track_id: String,
        result: Result<ResolvedAudio, String>,
    },
    RadioLoaded {
        request_id: u64,
        result: Result<RadioPage, String>,
        fallback_seed: Option<RadioSeed>,
    },
    AuthProgress(String),
    AuthDone(Result<String, String>),
    SessionVerified(Result<(), String>),
    OAuthStarted(Result<OAuthSession, String>),
    OAuthPolled(Result<bool, String>),
    UpdateAvailable(UpdateInfo),
    UpdateFinished(Result<String, String>),
    TrackRatingLoaded {
        track_id: String,
        result: Result<LikeStatus, String>,
    },
    TrackRatingSaved {
        track_id: String,
        target: LikeStatus,
        previous: Option<LikeStatus>,
        result: Result<(), String>,
    },
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
    }
}

struct TuiApp {
    paths: AppPaths,
    settings: AppSettings,
    ytm: YtmClient,
    audio: AudioEngine,
    media_keys: MediaKeysHandle,
    queue: PlaybackQueue,
    tx: mpsc::UnboundedSender<AsyncEvent>,

    screen: Screen,
    tab: Tab,
    focus: FocusPane,
    input_mode: InputMode,
    search_input: String,
    searching: bool,

    songs: Vec<Track>,
    albums: Vec<AlbumSummary>,
    selected_album: Option<AlbumDetail>,
    loading_album: bool,

    library_section: LibrarySection,
    library_tracks: Vec<Track>,
    library_albums: Vec<AlbumSummary>,
    loading_library: bool,

    catalog_state: ListState,
    queue_state: ListState,

    playback_status: PlaybackStatus,
    position: Duration,
    duration: Option<Duration>,
    codec_label: Option<String>,
    play_request_id: u64,
    radio_request_id: u64,
    prefetched: Option<(String, ResolvedAudio)>,
    prefetch_in_flight: Option<String>,
    status_line: String,

    auth_status: AuthStatus,
    session_expired: bool,
    detected_browser_name: Option<String>,
    login_cancel: Option<oneshot::Sender<()>>,
    cookie_input: String,
    delete_cookie_file: bool,
    auth_busy: bool,
    auth_feedback: Option<String>,
    oauth_pending: Option<OAuthSession>,
    last_oauth_poll: Instant,

    available_update: Option<UpdateInfo>,
    updating: bool,

    track_ratings: HashMap<String, LikeStatus>,
    rating_loading_for: Option<String>,
    rating_saving: bool,

    should_quit: bool,
}

impl TuiApp {
    fn new(
        paths: AppPaths,
        settings: AppSettings,
        ytm: YtmClient,
        audio: AudioEngine,
        media_keys: MediaKeysHandle,
        tx: mpsc::UnboundedSender<AsyncEvent>,
    ) -> Self {
        cleanup_stale_auth_profiles(&paths.data_dir);
        cleanup_previous_update_artifacts();

        let auth_status = ytm.auth_status();
        let detected_browser_name = detect_chromium_browser().map(|b| b.name);
        let screen = if auth_status.has_cookie || auth_status.has_oauth {
            Screen::Main
        } else {
            Screen::Login
        };
        let tab = if auth_status.has_cookie {
            Tab::Library
        } else {
            Tab::Songs
        };
        let status_line = if auth_status.has_cookie {
            "DPAPI SESSION LOADED // VERIFYING...".to_string()
        } else if auth_status.has_oauth {
            "OAUTH TV ACTIVE // PRESS '/' TO SEARCH".to_string()
        } else {
            "READY // SIGN IN WITH GOOGLE OR PRESS [ESC] FOR GUEST MODE".to_string()
        };

        let mut catalog_state = ListState::default();
        catalog_state.select(Some(0));
        let mut queue_state = ListState::default();
        queue_state.select(Some(0));

        let mut app = Self {
            paths,
            settings: settings.clone(),
            ytm,
            audio,
            media_keys,
            queue: PlaybackQueue::new(settings.autoplay_enabled),
            tx,
            screen,
            tab,
            focus: FocusPane::Catalog,
            input_mode: InputMode::Normal,
            search_input: String::new(),
            searching: false,
            songs: Vec::new(),
            albums: Vec::new(),
            selected_album: None,
            loading_album: false,
            library_section: LibrarySection::History,
            library_tracks: Vec::new(),
            library_albums: Vec::new(),
            loading_library: false,
            catalog_state,
            queue_state,
            playback_status: PlaybackStatus::Idle,
            position: Duration::ZERO,
            duration: None,
            codec_label: None,
            play_request_id: 0,
            radio_request_id: 0,
            prefetched: None,
            prefetch_in_flight: None,
            status_line,
            auth_status,
            session_expired: false,
            detected_browser_name,
            login_cancel: None,
            cookie_input: String::new(),
            delete_cookie_file: true,
            auth_busy: false,
            auth_feedback: None,
            oauth_pending: None,
            last_oauth_poll: Instant::now(),
            available_update: None,
            updating: false,
            track_ratings: HashMap::new(),
            rating_loading_for: None,
            rating_saving: false,
            should_quit: false,
        };

        // Comprobación silenciosa de actualizaciones en GitHub Releases al arrancar
        let tx_upd = app.tx.clone();
        tokio::spawn(async move {
            if let Some(info) = check_for_updates().await {
                let _ = tx_upd.send(AsyncEvent::UpdateAvailable(info));
            }
        });

        if auth_status.has_cookie {
            app.load_library(LibrarySection::History);
            let ytm_check = app.ytm.clone();
            let tx_check = app.tx.clone();
            tokio::spawn(async move {
                let res = ytm_check
                    .verify_cookie_session()
                    .await
                    .map_err(|e| e.to_string());
                let _ = tx_check.send(AsyncEvent::SessionVerified(res));
            });
        }
        app
    }

    fn start_browser_login(&mut self) {
        if self.auth_busy {
            return;
        }
        let (cancel_tx, cancel_rx) = oneshot::channel();
        self.login_cancel = Some(cancel_tx);
        self.auth_busy = true;
        self.session_expired = false;
        self.auth_feedback =
            Some("OPENING GOOGLE SIGN-IN WINDOW... [ESC TO CANCEL]".to_string());
        self.status_line = "GOOGLE SIGN-IN IN PROGRESS... [ESC TO CANCEL]".to_string();

        let ytm = self.ytm.clone();
        let data_dir = self.paths.data_dir.clone();
        let tx = self.tx.clone();
        let tx_prog = self.tx.clone();

        tokio::spawn(async move {
            let res = ytm
                .login_with_browser(&data_dir, cancel_rx, move |msg| {
                    let _ = tx_prog.send(AsyncEvent::AuthProgress(msg));
                })
                .await
                .map(|st| {
                    format!(
                        "GOOGLE AUTH OK // SAVED IN DPAPI ({} KEPT, {} STRIPPED)",
                        st.kept, st.dropped
                    )
                })
                .map_err(|e| e.to_string());
            let _ = tx.send(AsyncEvent::AuthDone(res));
        });
    }

    fn cancel_browser_login(&mut self) -> bool {
        if let Some(cancel_tx) = self.login_cancel.take() {
            let _ = cancel_tx.send(());
            self.auth_feedback = Some("CANCELLING GOOGLE SIGN-IN...".to_string());
            true
        } else {
            false
        }
    }

    fn start_cookie_file_import(&mut self) {
        if self.auth_busy {
            return;
        }
        let ytm = self.ytm.clone();
        let tx = self.tx.clone();
        let del = self.delete_cookie_file;
        self.auth_busy = true;
        self.auth_feedback = Some("OPENING FILE DIALOG FOR cookies.txt...".into());
        tokio::spawn(async move {
            let picked = rfd::AsyncFileDialog::new()
                .set_title("Seleccionar cookies.txt de YouTube Music")
                .add_filter("Cookies", &["txt"])
                .pick_file()
                .await
                .map(|f| f.path().to_path_buf());
            if let Some(path) = picked {
                let res = ytm
                    .login_with_cookie_file(&path, del)
                    .await
                    .map(|st| {
                        format!(
                            "AUTH OK // DPAPI ({} KEPT, {} STRIPPED){}",
                            st.kept,
                            st.dropped,
                            if del { " // .TXT WIPED" } else { "" }
                        )
                    })
                    .map_err(|e| e.to_string());
                let _ = tx.send(AsyncEvent::AuthDone(res));
            } else {
                let _ = tx.send(AsyncEvent::AuthDone(Ok("IMPORT CANCELLED".into())));
            }
        });
    }

    fn start_oauth_login(&mut self) {
        if self.auth_busy {
            return;
        }
        self.auth_busy = true;
        self.auth_feedback = Some("REQUESTING GOOGLE TV OAUTH CODE...".into());
        let ytm = self.ytm.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let res = ytm.start_oauth().await.map_err(|e| e.to_string());
            let _ = tx.send(AsyncEvent::OAuthStarted(res));
        });
    }

    fn start_logout(&mut self) {
        if self.auth_busy {
            return;
        }
        self.auth_busy = true;
        self.oauth_pending = None;
        self.session_expired = false;
        let ytm = self.ytm.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let res = ytm
                .logout_all()
                .await
                .map(|_| "SESSION CLEARED FROM DPAPI STORAGE".to_string())
                .map_err(|e| e.to_string());
            let _ = tx.send(AsyncEvent::AuthDone(res));
        });
    }

    fn start_update(&mut self) {
        if self.updating {
            return;
        }
        let Some(info) = self.available_update.clone() else {
            return;
        };
        self.updating = true;
        let msg = format!("DOWNLOADING UPDATE v{}...", info.version);
        self.status_line = msg.clone();
        if self.screen == Screen::Login || self.tab == Tab::Auth {
            self.auth_feedback = Some(msg);
        }

        let tx = self.tx.clone();
        tokio::spawn(async move {
            let res = download_and_install_update(&info)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(AsyncEvent::UpdateFinished(res));
        });
    }

    fn spawn_search(&mut self) {
        let q = self.search_input.trim().to_string();
        if q.is_empty() {
            return;
        }
        self.searching = true;
        self.selected_album = None;
        if matches!(self.tab, Tab::Auth | Tab::Library) {
            self.tab = Tab::Songs;
        }
        self.status_line = format!("SEARCHING: '{q}'...");

        let ytm1 = self.ytm.clone();
        let tx1 = self.tx.clone();
        let q1 = q.clone();
        tokio::spawn(async move {
            let res = ytm1.search_songs(&q1).await.map_err(|e| e.to_string());
            let _ = tx1.send(AsyncEvent::SongsLoaded(res));
        });

        let ytm2 = self.ytm.clone();
        let tx2 = self.tx.clone();
        tokio::spawn(async move {
            let res = ytm2.search_albums(&q).await.map_err(|e| e.to_string());
            let _ = tx2.send(AsyncEvent::AlbumsLoaded(res));
        });
    }

    fn load_library(&mut self, sec: LibrarySection) {
        if !self.auth_status.has_cookie {
            return;
        }
        self.library_section = sec;
        self.loading_library = true;
        self.selected_album = None;
        let ytm = self.ytm.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            match sec {
                LibrarySection::History => {
                    let res = ytm.get_history().await.map_err(|e| e.to_string());
                    let _ = tx.send(AsyncEvent::LibraryTracksLoaded(sec, res));
                }
                LibrarySection::Liked => {
                    let res = ytm.get_liked_tracks().await.map_err(|e| e.to_string());
                    let _ = tx.send(AsyncEvent::LibraryTracksLoaded(sec, res));
                }
                LibrarySection::SavedAlbums => {
                    let res = ytm.get_saved_albums().await.map_err(|e| e.to_string());
                    let _ = tx.send(AsyncEvent::LibraryAlbumsLoaded(res));
                }
            }
        });
    }

    fn open_album(&mut self, album_id: String) {
        self.loading_album = true;
        self.status_line = "LOADING ALBUM TRACKS...".to_string();
        let ytm = self.ytm.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let res = ytm.get_album(&album_id).await.map_err(|e| e.to_string());
            let _ = tx.send(AsyncEvent::AlbumDetailLoaded(res));
        });
    }

    fn start_playing_track(&mut self, track: Track) {
        self.play_request_id += 1;
        let req_id = self.play_request_id;
        self.position = Duration::ZERO;
        self.duration = track.duration_secs.map(|s| Duration::from_secs(s.into()));
        self.playback_status = PlaybackStatus::Buffering;
        self.status_line = format!("BUFFERING: {} — {}", track.title, track.artist);
        self.media_keys.update_metadata(&track, self.duration);
        self.media_keys.update_playback(true, false, Some(Duration::ZERO));

        if self.auth_status.has_cookie {
            self.rating_loading_for = Some(track.id.clone());
            let ytm_like = self.ytm.clone();
            let tx_like = self.tx.clone();
            let tid_like = track.id.clone();
            tokio::spawn(async move {
                let res = ytm_like
                    .get_track_like_status(&tid_like)
                    .await
                    .map_err(|e| e.to_string());
                let _ = tx_like.send(AsyncEvent::TrackRatingLoaded {
                    track_id: tid_like,
                    result: res,
                });
            });
        } else {
            self.rating_loading_for = None;
        }

        if let Some((cached_id, stream)) = self.prefetched.take()
            && cached_id == track.id
        {
            self.codec_label = Some(stream.codec_label.clone());
            if let Some(secs) = stream.duration_secs {
                self.duration = Some(Duration::from_secs(secs.into()));
            }
            self.audio.play_stream(track.id, stream);
            return;
        }

        let ytm = self.ytm.clone();
        let tx = self.tx.clone();
        let tid = track.id;
        tokio::spawn(async move {
            let res = ytm
                .resolve_audio_stream(&tid)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(AsyncEvent::StreamResolved {
                request_id: req_id,
                track_id: tid,
                result: res,
            });
        });
    }

    fn dispatch_radio(&mut self, req: RadioFetchRequest) {
        match req {
            RadioFetchRequest::None => {}
            RadioFetchRequest::Initial(seed) => {
                self.radio_request_id += 1;
                let rid = self.radio_request_id;
                let ytm = self.ytm.clone();
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    let res = match seed {
                        RadioSeed::Track(vid) => ytm.get_radio_for_track(&vid).await,
                        RadioSeed::Playlist(pid) => ytm.get_radio_for_playlist(&pid).await,
                    }
                    .map_err(|e| e.to_string());
                    let _ = tx.send(AsyncEvent::RadioLoaded {
                        request_id: rid,
                        result: res,
                        fallback_seed: None,
                    });
                });
            }
            RadioFetchRequest::Continuation {
                token,
                visitor_data,
                fallback_seed,
            } => {
                self.radio_request_id += 1;
                let rid = self.radio_request_id;
                let ytm = self.ytm.clone();
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    let res = ytm
                        .get_radio_continuation(&token, visitor_data.as_deref())
                        .await
                        .map_err(|e| e.to_string());
                    let _ = tx.send(AsyncEvent::RadioLoaded {
                        request_id: rid,
                        result: res,
                        fallback_seed,
                    });
                });
            }
        }
    }

    fn advance_next(&mut self) {
        if let Some((next_track, radio_req)) = self.queue.advance_next() {
            self.start_playing_track(next_track);
            self.dispatch_radio(radio_req);
        } else {
            let req = self.queue.check_refill_needed();
            if req != RadioFetchRequest::None {
                self.status_line = "FETCHING GOOGLE RADIO...".to_string();
                self.dispatch_radio(req);
            } else {
                self.playback_status = PlaybackStatus::Idle;
            }
        }
    }

    fn catalog_len(&self) -> usize {
        if let Some(alb) = &self.selected_album {
            return alb.tracks.len();
        }
        match self.tab {
            Tab::Songs => self.songs.len(),
            Tab::Albums => self.albums.len(),
            Tab::Library => match self.library_section {
                LibrarySection::SavedAlbums => self.library_albums.len(),
                _ => self.library_tracks.len(),
            },
            Tab::Auth => 0,
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let len = match self.focus {
            FocusPane::Catalog => self.catalog_len(),
            FocusPane::Queue => self.queue.upcoming().len(),
        };
        let state = match self.focus {
            FocusPane::Catalog => &mut self.catalog_state,
            FocusPane::Queue => &mut self.queue_state,
        };
        if len == 0 {
            state.select(None);
            return;
        }
        let cur = state.selected().unwrap_or(0) as isize;
        let next = (cur + delta).clamp(0, (len - 1) as isize) as usize;
        state.select(Some(next));
    }

    fn activate_selected(&mut self) {
        match self.focus {
            FocusPane::Queue => {
                if let Some(idx) = self.queue_state.selected()
                    && let Some((track, radio_req)) = self.queue.jump_to_upcoming(idx)
                {
                    self.start_playing_track(track);
                    self.dispatch_radio(radio_req);
                }
            }
            FocusPane::Catalog => {
                let Some(idx) = self.catalog_state.selected() else {
                    return;
                };
                if let Some(alb) = self.selected_album.clone() {
                    if idx < alb.tracks.len() {
                        let req = self
                            .queue
                            .play_track_list(alb.tracks, idx, alb.playlist_id);
                        if let Some(curr) = self.queue.current().cloned() {
                            self.start_playing_track(curr);
                        }
                        self.dispatch_radio(req);
                    }
                    return;
                }
                match self.tab {
                    Tab::Songs => {
                        if let Some(track) = self.songs.get(idx).cloned() {
                            let req = self.queue.play_single_track(track.clone());
                            self.start_playing_track(track);
                            self.dispatch_radio(req);
                        }
                    }
                    Tab::Albums => {
                        if let Some(alb) = self.albums.get(idx) {
                            self.open_album(alb.id.clone());
                        }
                    }
                    Tab::Library => match self.library_section {
                        LibrarySection::SavedAlbums => {
                            if let Some(alb) = self.library_albums.get(idx) {
                                self.open_album(alb.id.clone());
                            }
                        }
                        _ => {
                            if let Some(track) = self.library_tracks.get(idx).cloned() {
                                let req = self.queue.play_single_track(track.clone());
                                self.start_playing_track(track);
                                self.dispatch_radio(req);
                            }
                        }
                    },
                    Tab::Auth => {}
                }
            }
        }
    }

    fn enqueue_selected(&mut self) {
        if self.focus != FocusPane::Catalog {
            return;
        }
        let Some(idx) = self.catalog_state.selected() else {
            return;
        };
        let track = if let Some(alb) = &self.selected_album {
            alb.tracks.get(idx).cloned()
        } else {
            match self.tab {
                Tab::Songs => self.songs.get(idx).cloned(),
                Tab::Library if self.library_section != LibrarySection::SavedAlbums => {
                    self.library_tracks.get(idx).cloned()
                }
                _ => None,
            }
        };
        if let Some(t) = track {
            self.status_line = format!("QUEUED: {}", t.title);
            self.queue.enqueue_track(t);
        }
    }

    fn rate_current_track(&mut self, desired: LikeStatus) {
        if !self.auth_status.has_cookie {
            self.status_line =
                "LOGIN REQUIRED // PRESS [4] TO SIGN IN WITH GOOGLE TO RATE".to_string();
            return;
        }
        if self.rating_saving {
            return;
        }
        let Some(curr) = self.queue.current().cloned() else {
            return;
        };
        let previous = self.track_ratings.get(&curr.id).copied();
        let target = if previous == Some(desired) {
            LikeStatus::Indifferent
        } else {
            desired
        };
        self.track_ratings.insert(curr.id.clone(), target);
        self.rating_saving = true;

        let ytm = self.ytm.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let res = ytm
                .rate_track(&curr.id, target)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(AsyncEvent::TrackRatingSaved {
                track_id: curr.id,
                target,
                previous,
                result: res,
            });
        });
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.cancel_browser_login();
            self.should_quit = true;
            return;
        }

        if self.screen == Screen::Login && self.input_mode == InputMode::Normal {
            match key.code {
                KeyCode::Char('q') => {
                    self.cancel_browser_login();
                    self.should_quit = true;
                }
                KeyCode::Enter | KeyCode::Char('g') => self.start_browser_login(),
                KeyCode::Char('u') if self.available_update.is_some() => self.start_update(),
                KeyCode::Char('o') => self.start_oauth_login(),
                KeyCode::Char('i') => self.start_cookie_file_import(),
                KeyCode::Char('c') if !self.auth_busy => {
                    self.cookie_input.zeroize();
                    self.input_mode = InputMode::PasteCookie;
                }
                KeyCode::Esc => {
                    if !self.cancel_browser_login() && !self.auth_busy {
                        self.screen = Screen::Main;
                        self.status_line =
                            "GUEST MODE // PRESS '/' TO SEARCH OR '4' TO SIGN IN".to_string();
                    }
                }
                KeyCode::Char('s') if !self.auth_busy => {
                    self.screen = Screen::Main;
                    self.status_line =
                        "GUEST MODE // PRESS '/' TO SEARCH OR '4' TO SIGN IN".to_string();
                }
                _ => {}
            }
            return;
        }

        match self.input_mode {
            InputMode::Search => match key.code {
                KeyCode::Esc => self.input_mode = InputMode::Normal,
                KeyCode::Enter => {
                    self.input_mode = InputMode::Normal;
                    self.spawn_search();
                }
                KeyCode::Backspace => {
                    self.search_input.pop();
                }
                KeyCode::Char(c) => self.search_input.push(c),
                _ => {}
            },
            InputMode::PasteCookie => match key.code {
                KeyCode::Esc => {
                    self.cookie_input.zeroize();
                    self.input_mode = InputMode::Normal;
                }
                KeyCode::Enter => {
                    self.input_mode = InputMode::Normal;
                    let raw = Zeroizing::new(std::mem::take(&mut self.cookie_input));
                    self.cookie_input.zeroize();
                    if !raw.trim().is_empty() && !self.auth_busy {
                        self.auth_busy = true;
                        self.auth_feedback = Some("FILTERING & VALIDATING SESSION...".into());
                        let ytm = self.ytm.clone();
                        let tx = self.tx.clone();
                        tokio::spawn(async move {
                            let res = ytm
                                .login_with_cookie_input(&raw)
                                .await
                                .map(|st| {
                                    format!(
                                        "AUTH OK // DPAPI ({} KEPT, {} STRIPPED)",
                                        st.kept, st.dropped
                                    )
                                })
                                .map_err(|e| e.to_string());
                            let _ = tx.send(AsyncEvent::AuthDone(res));
                        });
                    }
                }
                KeyCode::Backspace => {
                    self.cookie_input.pop();
                }
                KeyCode::Char(c) => self.cookie_input.push(c),
                _ => {}
            },
            InputMode::Normal => match key.code {
                KeyCode::Char('q') => {
                    self.cancel_browser_login();
                    self.should_quit = true;
                }
                KeyCode::Char('/') | KeyCode::Char('s') => {
                    self.input_mode = InputMode::Search;
                }
                KeyCode::Char('1') | KeyCode::F(1) => {
                    self.tab = Tab::Songs;
                    self.selected_album = None;
                    self.focus = FocusPane::Catalog;
                    self.catalog_state.select(Some(0));
                }
                KeyCode::Char('2') | KeyCode::F(2) => {
                    self.tab = Tab::Albums;
                    self.selected_album = None;
                    self.focus = FocusPane::Catalog;
                    self.catalog_state.select(Some(0));
                }
                KeyCode::Char('3') | KeyCode::F(3) => {
                    self.tab = Tab::Library;
                    self.selected_album = None;
                    self.focus = FocusPane::Catalog;
                    self.catalog_state.select(Some(0));
                    self.load_library(self.library_section);
                }
                KeyCode::Char('4') | KeyCode::F(4) => {
                    self.tab = Tab::Auth;
                    self.selected_album = None;
                    self.focus = FocusPane::Catalog;
                }
                KeyCode::Tab => {
                    self.focus = match self.focus {
                        FocusPane::Catalog => FocusPane::Queue,
                        FocusPane::Queue => FocusPane::Catalog,
                    };
                }
                KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
                KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
                KeyCode::Enter => self.activate_selected(),
                KeyCode::Esc | KeyCode::Backspace => {
                    if key.code == KeyCode::Esc && self.cancel_browser_login() {
                        return;
                    }
                    if self.selected_album.is_some() {
                        self.selected_album = None;
                        self.catalog_state.select(Some(0));
                    }
                }
                KeyCode::Char(' ') => match self.playback_status {
                    PlaybackStatus::Playing => {
                        self.audio.pause();
                        self.playback_status = PlaybackStatus::Paused;
                    }
                    PlaybackStatus::Paused => {
                        self.audio.resume();
                        self.playback_status = PlaybackStatus::Playing;
                    }
                    PlaybackStatus::Idle | PlaybackStatus::Error(_) => {
                        if let Some(curr) = self.queue.current().cloned() {
                            self.start_playing_track(curr);
                        }
                    }
                    PlaybackStatus::Buffering => {}
                },
                KeyCode::Char('n') => self.advance_next(),
                KeyCode::Char('p') => {
                    if self.position > Duration::from_secs(4) {
                        self.audio.seek(Duration::ZERO);
                        self.position = Duration::ZERO;
                    } else if let Some(prev) = self.queue.go_previous() {
                        self.start_playing_track(prev);
                    }
                }
                KeyCode::Left => {
                    let target = self.position.saturating_sub(Duration::from_secs(10));
                    self.position = target;
                    self.audio.seek(target);
                }
                KeyCode::Right => {
                    let max = self.duration.unwrap_or(Duration::from_secs(3600));
                    let target = (self.position + Duration::from_secs(10)).min(max);
                    self.position = target;
                    self.audio.seek(target);
                }
                KeyCode::Char('+') | KeyCode::Char('=') => {
                    self.settings.volume = (self.settings.volume + 0.05).clamp(0.0, 1.0);
                    self.audio.set_volume(self.settings.volume);
                    self.paths.save_settings(&self.settings);
                }
                KeyCode::Char('-') => {
                    self.settings.volume = (self.settings.volume - 0.05).clamp(0.0, 1.0);
                    self.audio.set_volume(self.settings.volume);
                    self.paths.save_settings(&self.settings);
                }
                KeyCode::Char('a') => {
                    let next = !self.queue.autoplay_enabled;
                    self.settings.autoplay_enabled = next;
                    self.paths.save_settings(&self.settings);
                    let req = self.queue.set_autoplay(next);
                    self.dispatch_radio(req);
                }
                KeyCode::Char('L') | KeyCode::Char('f') => {
                    self.rate_current_track(LikeStatus::Like);
                }
                KeyCode::Char('D') => {
                    self.rate_current_track(LikeStatus::Dislike);
                }
                KeyCode::Char('u') if self.available_update.is_some() => {
                    self.start_update();
                }
                KeyCode::Char('e') => self.enqueue_selected(),
                KeyCode::Char('d') | KeyCode::Char('x') if self.focus == FocusPane::Queue => {
                    if let Some(idx) = self.queue_state.selected() {
                        self.queue.remove_upcoming(idx);
                        let req = self.queue.check_refill_needed();
                        self.dispatch_radio(req);
                    }
                }
                // Subcomandos de Biblioteca
                KeyCode::Char('h') if self.tab == Tab::Library => {
                    self.load_library(LibrarySection::History);
                }
                KeyCode::Char('l') if self.tab == Tab::Library => {
                    self.load_library(LibrarySection::Liked);
                }
                KeyCode::Char('b') if self.tab == Tab::Library => {
                    self.load_library(LibrarySection::SavedAlbums);
                }
                // Subcomandos de Autenticación / DPAPI
                KeyCode::Char('g') if self.tab == Tab::Auth && !self.auth_busy => {
                    self.start_browser_login();
                }
                KeyCode::Char('i') if self.tab == Tab::Auth && !self.auth_busy => {
                    self.start_cookie_file_import();
                }
                KeyCode::Char('c') if self.tab == Tab::Auth && !self.auth_busy => {
                    self.cookie_input.zeroize();
                    self.input_mode = InputMode::PasteCookie;
                }
                KeyCode::Char('t') if self.tab == Tab::Auth => {
                    self.delete_cookie_file = !self.delete_cookie_file;
                }
                KeyCode::Char('o') if self.tab == Tab::Auth && !self.auth_busy => {
                    self.start_oauth_login();
                }
                KeyCode::Char('x') if self.tab == Tab::Auth && !self.auth_busy => {
                    self.start_logout();
                }
                _ => {}
            },
        }
    }

    fn handle_async(&mut self, ev: AsyncEvent) {
        match ev {
            AsyncEvent::SongsLoaded(res) => {
                self.searching = false;
                match res {
                    Ok(tracks) => {
                        self.status_line = format!("FOUND {} SONGS", tracks.len());
                        self.songs = tracks;
                        self.catalog_state.select(Some(0));
                    }
                    Err(e) => self.status_line = format!("ERR: {e}"),
                }
            }
            AsyncEvent::AlbumsLoaded(res) => {
                self.searching = false;
                match res {
                    Ok(albums) => self.albums = albums,
                    Err(e) => self.status_line = format!("ERR: {e}"),
                }
            }
            AsyncEvent::AlbumDetailLoaded(res) => {
                self.loading_album = false;
                match res {
                    Ok(detail) => {
                        self.status_line = format!(
                            "ALBUM: {} ({} TRACKS)",
                            detail.summary.title,
                            detail.tracks.len()
                        );
                        self.selected_album = Some(detail);
                        self.catalog_state.select(Some(0));
                    }
                    Err(e) => self.status_line = format!("ERR: {e}"),
                }
            }
            AsyncEvent::LibraryTracksLoaded(sec, res) => {
                self.loading_library = false;
                if sec == self.library_section {
                    match res {
                        Ok(tracks) => {
                            if sec == LibrarySection::Liked {
                                for t in &tracks {
                                    self.track_ratings.insert(t.id.clone(), LikeStatus::Like);
                                }
                            }
                            self.library_tracks = tracks;
                            self.catalog_state.select(Some(0));
                        }
                        Err(e) => self.status_line = format!("LIB ERR: {e}"),
                    }
                }
            }
            AsyncEvent::LibraryAlbumsLoaded(res) => {
                self.loading_library = false;
                match res {
                    Ok(albums) => {
                        self.library_albums = albums;
                        self.catalog_state.select(Some(0));
                    }
                    Err(e) => self.status_line = format!("LIB ERR: {e}"),
                }
            }
            AsyncEvent::StreamResolved {
                request_id,
                track_id,
                result,
            } => {
                if request_id != self.play_request_id {
                    return;
                }
                match result {
                    Ok(stream) => {
                        self.codec_label = Some(stream.codec_label.clone());
                        if let Some(secs) = stream.duration_secs {
                            self.duration = Some(Duration::from_secs(secs.into()));
                        }
                        self.audio.play_stream(track_id, stream);
                    }
                    Err(e) => {
                        self.playback_status = PlaybackStatus::Error(e.clone());
                        self.status_line = format!("PLAY ERR: {e}");
                    }
                }
            }
            AsyncEvent::PrefetchResolved { track_id, result } => {
                if self.prefetch_in_flight.as_deref() == Some(track_id.as_str()) {
                    self.prefetch_in_flight = None;
                }
                if let Ok(stream) = result {
                    self.prefetched = Some((track_id, stream));
                }
            }
            AsyncEvent::RadioLoaded {
                request_id,
                result,
                fallback_seed,
            } => {
                if request_id != self.radio_request_id {
                    return;
                }
                match result {
                    Ok(page) => {
                        let added = self.queue.apply_radio_page(page);
                        if added > 0 {
                            self.status_line = format!("GOOGLE RADIO // +{added} RECOMMENDATIONS");
                        }
                    }
                    Err(e) => {
                        self.queue.set_radio_loading(false);
                        if let Some(seed) = fallback_seed {
                            self.queue.set_radio_loading(true);
                            self.dispatch_radio(RadioFetchRequest::Initial(seed));
                        } else {
                            self.status_line = format!("RADIO ERR: {e}");
                        }
                    }
                }
            }
            AsyncEvent::AuthProgress(msg) => {
                self.auth_feedback = Some(msg.clone());
                self.status_line = msg;
            }
            AsyncEvent::SessionVerified(res) => match res {
                Ok(()) => {
                    self.session_expired = false;
                    self.status_line =
                        "DPAPI SESSION VERIFIED // PERSONALIZED RADIO READY".to_string();
                }
                Err(_) => {
                    self.session_expired = true;
                    self.status_line =
                        "SESSION EXPIRED // PRESS [4] THEN [g] TO SIGN IN WITH GOOGLE".to_string();
                }
            },
            AsyncEvent::AuthDone(res) => {
                self.auth_busy = false;
                self.login_cancel = None;
                self.auth_status = self.ytm.auth_status();
                match res {
                    Ok(msg) => {
                        self.auth_feedback = Some(msg.clone());
                        self.status_line = msg;
                        if self.auth_status.has_cookie {
                            self.session_expired = false;
                            if self.screen == Screen::Login {
                                self.screen = Screen::Main;
                                self.tab = Tab::Library;
                            }
                            self.load_library(self.library_section);
                        } else {
                            self.library_tracks.clear();
                            self.library_albums.clear();
                            self.track_ratings.clear();
                            self.rating_loading_for = None;
                        }
                    }
                    Err(e) => self.auth_feedback = Some(format!("ERR // {e}")),
                }
            }
            AsyncEvent::OAuthStarted(res) => {
                self.auth_busy = false;
                match res {
                    Ok(session) => {
                        let opened =
                            open_url_in_default_browser(&session.code.verification_url).is_ok();
                        let prefix = if opened { "BROWSER OPENED" } else { "OPEN" };
                        self.auth_feedback = Some(format!(
                            "{prefix} {} // ENTER CODE: {}",
                            session.code.verification_url, session.code.user_code
                        ));
                        self.oauth_pending = Some(session);
                        self.last_oauth_poll = Instant::now();
                    }
                    Err(e) => self.auth_feedback = Some(format!("OAUTH ERR // {e}")),
                }
            }
            AsyncEvent::OAuthPolled(res) => match res {
                Ok(true) => {
                    self.oauth_pending = None;
                    self.auth_status = self.ytm.auth_status();
                    self.auth_feedback = Some("OAUTH GOOGLE TV ACTIVE // DPAPI SAVED".into());
                    self.status_line = "OAUTH GOOGLE TV ACTIVE // DPAPI SAVED".into();
                    if self.screen == Screen::Login {
                        self.screen = Screen::Main;
                        self.tab = Tab::Songs;
                    }
                }
                Ok(false) => {}
                Err(e) => {
                    self.oauth_pending = None;
                    self.auth_feedback = Some(format!("OAUTH ENDED // {e}"));
                }
            },
            AsyncEvent::UpdateAvailable(info) => {
                let notice = format!(
                    "UPDATE AVAILABLE: v{} (CURRENT v{}) // PRESS [u] TO UPDATE",
                    info.version, CURRENT_VERSION
                );
                self.available_update = Some(info);
                self.status_line = notice;
            }
            AsyncEvent::UpdateFinished(res) => {
                self.updating = false;
                match res {
                    Ok(msg) => {
                        self.available_update = None;
                        self.status_line = msg.clone();
                        if self.screen == Screen::Login || self.tab == Tab::Auth {
                            self.auth_feedback = Some(msg);
                        }
                    }
                    Err(e) => {
                        let err_msg = format!("UPDATE ERR // {e}");
                        self.status_line = err_msg.clone();
                        if self.screen == Screen::Login || self.tab == Tab::Auth {
                            self.auth_feedback = Some(err_msg);
                        }
                    }
                }
            }
            AsyncEvent::TrackRatingLoaded { track_id, result } => {
                if self.rating_loading_for.as_deref() == Some(track_id.as_str()) {
                    self.rating_loading_for = None;
                }
                if let Ok(status) = result {
                    self.track_ratings.insert(track_id, status);
                }
            }
            AsyncEvent::TrackRatingSaved {
                track_id,
                target,
                previous,
                result,
            } => {
                self.rating_saving = false;
                match result {
                    Ok(()) => {
                        self.track_ratings.insert(track_id.clone(), target);
                        if self.library_section == LibrarySection::Liked
                            && target != LikeStatus::Like
                        {
                            self.library_tracks.retain(|t| t.id != track_id);
                            let len = self.library_tracks.len();
                            if len == 0 {
                                self.catalog_state.select(None);
                            } else if let Some(sel) = self.catalog_state.selected()
                                && sel >= len
                            {
                                self.catalog_state.select(Some(len - 1));
                            }
                        }
                    }
                    Err(e) => {
                        match previous {
                            Some(prev) => {
                                self.track_ratings.insert(track_id, prev);
                            }
                            None => {
                                self.track_ratings.remove(&track_id);
                            }
                        }
                        self.status_line = format!("RATE ERR: {e}");
                    }
                }
            }
        }
    }

    fn handle_os_media(&mut self, ev: OsMediaEvent) {
        match ev {
            OsMediaEvent::Play => {
                if self.playback_status == PlaybackStatus::Paused {
                    self.audio.resume();
                    self.playback_status = PlaybackStatus::Playing;
                    self.media_keys
                        .update_playback(true, false, Some(self.position));
                } else if matches!(
                    self.playback_status,
                    PlaybackStatus::Idle | PlaybackStatus::Error(_)
                ) && let Some(curr) = self.queue.current().cloned()
                {
                    self.start_playing_track(curr);
                }
            }
            OsMediaEvent::Pause => {
                if self.playback_status == PlaybackStatus::Playing {
                    self.audio.pause();
                    self.playback_status = PlaybackStatus::Paused;
                    self.media_keys
                        .update_playback(false, false, Some(self.position));
                }
            }
            OsMediaEvent::Toggle => match self.playback_status {
                PlaybackStatus::Playing => {
                    self.audio.pause();
                    self.playback_status = PlaybackStatus::Paused;
                    self.media_keys
                        .update_playback(false, false, Some(self.position));
                }
                PlaybackStatus::Paused => {
                    self.audio.resume();
                    self.playback_status = PlaybackStatus::Playing;
                    self.media_keys
                        .update_playback(true, false, Some(self.position));
                }
                PlaybackStatus::Idle | PlaybackStatus::Error(_) => {
                    if let Some(curr) = self.queue.current().cloned() {
                        self.start_playing_track(curr);
                    }
                }
                PlaybackStatus::Buffering => {}
            },
            OsMediaEvent::Next => self.advance_next(),
            OsMediaEvent::Previous => {
                if self.position > Duration::from_secs(4) {
                    self.audio.seek(Duration::ZERO);
                    self.position = Duration::ZERO;
                } else if let Some(prev) = self.queue.go_previous() {
                    self.start_playing_track(prev);
                }
            }
            OsMediaEvent::Stop => {
                self.audio.stop();
                self.playback_status = PlaybackStatus::Idle;
                self.position = Duration::ZERO;
                self.media_keys.update_playback(false, true, None);
            }
            OsMediaEvent::SeekTo(pos) => {
                self.position = pos;
                self.audio.seek(pos);
            }
            OsMediaEvent::SeekBySecs(delta) => {
                let target = if delta >= 0 {
                    let max = self.duration.unwrap_or(Duration::from_secs(3600));
                    (self.position + Duration::from_secs(delta as u64)).min(max)
                } else {
                    self.position
                        .saturating_sub(Duration::from_secs(delta.unsigned_abs()))
                };
                self.position = target;
                self.audio.seek(target);
            }
        }
    }

    fn tick_audio(&mut self) {
        let prev_status = self.playback_status.clone();
        let snap = self.audio.poll_snapshot();
        self.playback_status = snap.status;
        self.position = snap.position;
        if snap.duration.is_some() {
            self.duration = snap.duration;
            if let Some(curr) = self.queue.current() {
                self.media_keys.update_metadata(curr, self.duration);
            }
        }
        if snap.codec_label.is_some() {
            self.codec_label = snap.codec_label;
        }
        if self.playback_status != prev_status {
            match self.playback_status {
                PlaybackStatus::Playing => self
                    .media_keys
                    .update_playback(true, false, Some(self.position)),
                PlaybackStatus::Paused => self
                    .media_keys
                    .update_playback(false, false, Some(self.position)),
                PlaybackStatus::Idle | PlaybackStatus::Error(_) => {
                    self.media_keys.update_playback(false, true, None);
                }
                PlaybackStatus::Buffering => {}
            }
        }

        if let Some(ended_id) = snap.ended_track_id
            && self.queue.current().map(|t| t.id.as_str()) == Some(ended_id.as_str())
        {
            self.advance_next();
        }

        // Prefetch 20s antes del final
        if let Some(total) = self.duration
            && total > self.position
            && (total - self.position) <= Duration::from_secs(20)
            && let Some(next_track) = self.queue.upcoming().front()
        {
            let already_cached = self
                .prefetched
                .as_ref()
                .map(|(id, _)| id == &next_track.id)
                .unwrap_or(false);
            let in_flight = self.prefetch_in_flight.as_deref() == Some(next_track.id.as_str());
            if !already_cached && !in_flight {
                let tid = next_track.id.clone();
                self.prefetch_in_flight = Some(tid.clone());
                let ytm = self.ytm.clone();
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    let res = ytm
                        .resolve_audio_stream(&tid)
                        .await
                        .map_err(|e| e.to_string());
                    let _ = tx.send(AsyncEvent::PrefetchResolved {
                        track_id: tid,
                        result: res,
                    });
                });
            }
        }

        // Poll OAuth si está pendiente
        if let Some(session) = self.oauth_pending.clone() {
            let interval = Duration::from_secs(session.code.interval.max(3) as u64);
            if self.last_oauth_poll.elapsed() >= interval {
                self.last_oauth_poll = Instant::now();
                let ytm = self.ytm.clone();
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    let res = ytm.poll_oauth(&session).await.map_err(|e| e.to_string());
                    let _ = tx.send(AsyncEvent::OAuthPolled(res));
                });
            }
        }
    }

    fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        frame.render_widget(
            Block::default().style(Style::default().bg(BG_OBSIDIAN).fg(AMBER_MAIN)),
            area,
        );

        if self.screen == Screen::Login {
            self.draw_login_screen(frame, area);
            return;
        }

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(4), // Top console bar
                Constraint::Min(8),    // Main catalog + radio queue
                Constraint::Length(5), // Bottom audio deck bar
            ])
            .split(area);

        self.draw_header(frame, chunks[0]);

        let body = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
            .split(chunks[1]);

        self.draw_catalog(frame, body[0]);
        self.draw_queue(frame, body[1]);
        self.draw_player(frame, chunks[2]);
    }

    fn draw_login_screen(&self, frame: &mut Frame, area: Rect) {
        let card_w = area.width.saturating_sub(4).min(84);
        let card_h = area.height.saturating_sub(2).min(28);
        let x = area.x + area.width.saturating_sub(card_w) / 2;
        let y = area.y + area.height.saturating_sub(card_h) / 2;
        let card_area = Rect::new(x, y, card_w, card_h);

        let browser_hint = self
            .detected_browser_name
            .as_deref()
            .unwrap_or("No Chromium browser detected");
        let vault = self.paths.data_dir.join(ENCRYPTED_SESSION_FILE);

        let mut lines = vec![
            Line::from(Span::styled(
                format!("  YT-NIUSIC v{CURRENT_VERSION} // YOUTUBE MUSIC CONSOLE"),
                Style::default()
                    .fg(AMBER_BRIGHT)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                "  ────────────────────────────────────────────────────────────────────",
                Style::default().fg(BRONZE),
            )),
            Line::from(Span::styled(
                "  Inicia sesión con Google para acceder a Biblioteca, Historial, Me gusta",
                Style::default().fg(AMBER_MAIN),
            )),
            Line::from(Span::styled(
                "  y Radio personalizada. Queda cifrada con Windows DPAPI en este equipo.",
                Style::default().fg(AMBER_DIM),
            )),
            Line::from(""),
            Line::from(vec![
                Span::styled(
                    "  [Enter / g] ",
                    Style::default()
                        .bg(AMBER_MAIN)
                        .fg(BG_OBSIDIAN)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(" Iniciar sesión con Google ({browser_hint})  [RECOMENDADO]"),
                    Style::default()
                        .fg(AMBER_BRIGHT)
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
        ];

        if let Some(fb) = &self.auth_feedback {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                format!("  > {fb}"),
                Style::default()
                    .fg(if fb.starts_with("ERR") {
                        AMBER_ALERT
                    } else {
                        AMBER_BRIGHT
                    })
                    .add_modifier(Modifier::BOLD),
            )));
        }

        if let Some(upd) = &self.available_update {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                format!(
                    "  [u] NUEVA VERSIÓN DISPONIBLE: v{} (actual v{}) — Presiona [u] para actualizar",
                    upd.version, CURRENT_VERSION
                ),
                Style::default()
                    .bg(AMBER_MAIN)
                    .fg(BG_OBSIDIAN)
                    .add_modifier(Modifier::BOLD),
            )));
        }

        lines.extend([
            Line::from(""),
            Line::from(Span::styled(
                "  [o]  Vincular con código Google TV (abre google.com/device)",
                Style::default().fg(AMBER_MAIN),
            )),
            Line::from(Span::styled(
                "  [i]  Avanzado: Importar archivo cookies.txt",
                Style::default().fg(AMBER_DIM),
            )),
            Line::from(Span::styled(
                "  [c]  Avanzado: Pegar cabecera Cookie (enmascarado)",
                Style::default().fg(AMBER_DIM),
            )),
            Line::from(Span::styled(
                if self.login_cancel.is_some() {
                    "  [Esc] Cancelar inicio de sesión en navegador   [q] Salir"
                } else {
                    "  [s / Esc] Continuar sin cuenta (Modo Invitado) [q] Salir"
                },
                Style::default().fg(AMBER_DIM),
            )),
        ]);

        if self.input_mode == InputMode::PasteCookie {
            let masked = "*".repeat(self.cookie_input.len().min(42));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                format!("  COOKIE (Enter=Guardar, Esc=Cancelar) > {masked}_"),
                Style::default()
                    .bg(AMBER_MAIN)
                    .fg(BG_OBSIDIAN)
                    .add_modifier(Modifier::BOLD),
            )));
        }

        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("  DPAPI VAULT: {}", vault.display()),
            Style::default().fg(BRONZE),
        )));

        let block = Block::default()
            .title(Span::styled(
                " [ INICIO DE SESIÓN // GOOGLE + WINDOWS DPAPI ] ",
                Style::default()
                    .fg(AMBER_BRIGHT)
                    .add_modifier(Modifier::BOLD),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(AMBER_MAIN))
            .style(Style::default().bg(BG_PANEL));

        let p = Paragraph::new(lines).block(block).wrap(Wrap { trim: false });
        frame.render_widget(p, card_area);
    }

    fn draw_header(&self, frame: &mut Frame, area: Rect) {
        let auth_tag = if self.session_expired {
            "[4:AUTH:EXP]"
        } else if self.auth_status.has_cookie {
            "[4:AUTH:ON]"
        } else if self.auth_status.has_oauth {
            "[4:AUTH:TV]"
        } else {
            "[4:AUTH:OFF]"
        };

        let tab_span = |label: &'static str, active: bool| -> Span<'static> {
            if active {
                Span::styled(
                    format!(" {label} "),
                    Style::default()
                        .bg(AMBER_MAIN)
                        .fg(BG_OBSIDIAN)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Span::styled(format!(" {label} "), Style::default().fg(AMBER_DIM))
            }
        };

        let cursor = if self.input_mode == InputMode::Search {
            "_"
        } else {
            ""
        };
        let search_style = if self.input_mode == InputMode::Search {
            Style::default()
                .fg(AMBER_BRIGHT)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(AMBER_MAIN)
        };

        let line1 = Line::from(vec![
            Span::styled(
                format!(" YT-NIUSIC v{CURRENT_VERSION} // AUDIO CONSOLE "),
                Style::default()
                    .fg(AMBER_BRIGHT)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" │ ", Style::default().fg(BRONZE)),
            Span::styled(
                &self.status_line,
                Style::default().fg(if self.available_update.is_some() {
                    AMBER_BRIGHT
                } else {
                    AMBER_DIM
                }),
            ),
        ]);

        let line2 = Line::from(vec![
            Span::styled(" FIND > ", Style::default().fg(AMBER_BRIGHT)),
            Span::styled(format!("{}{cursor}", self.search_input), search_style),
            Span::styled("   ", Style::default()),
            tab_span("[1:SONGS]", self.tab == Tab::Songs),
            tab_span("[2:ALBUMS]", self.tab == Tab::Albums),
            tab_span("[3:LIBRARY]", self.tab == Tab::Library),
            tab_span(auth_tag, self.tab == Tab::Auth),
        ]);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(if self.input_mode == InputMode::Search {
                AMBER_MAIN
            } else {
                BRONZE
            }))
            .style(Style::default().bg(BG_PANEL));

        let p = Paragraph::new(vec![line1, line2]).block(block);
        frame.render_widget(p, area);
    }

    fn draw_catalog(&mut self, frame: &mut Frame, area: Rect) {
        let title = if self.loading_album {
            " [ CATALOG // LOADING ALBUM... ] "
        } else if let Some(alb) = &self.selected_album {
            // Título compacto
            if alb.summary.title.len() > 36 {
                " [ CATALOG // ALBUM DETAIL ] "
            } else {
                " [ CATALOG // ALBUM TRACKS (ESC: BACK) ] "
            }
        } else {
            match self.tab {
                Tab::Songs => " [ CATALOG // SONGS ] ",
                Tab::Albums => " [ CATALOG // ALBUMS ] ",
                Tab::Library => match self.library_section {
                    LibrarySection::History => " [ LIBRARY // HISTORY (h/l/b) ] ",
                    LibrarySection::Liked => " [ LIBRARY // LIKED (h/l/b) ] ",
                    LibrarySection::SavedAlbums => " [ LIBRARY // ALBUMS (h/l/b) ] ",
                },
                Tab::Auth => " [ SYSTEM // ACCOUNT & DPAPI ] ",
            }
        };

        let border_color = if self.focus == FocusPane::Catalog {
            AMBER_MAIN
        } else {
            BRONZE
        };

        let block = Block::default()
            .title(Span::styled(
                title,
                Style::default()
                    .fg(AMBER_BRIGHT)
                    .add_modifier(Modifier::BOLD),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color))
            .style(Style::default().bg(BG_PANEL));

        if self.tab == Tab::Auth && self.selected_album.is_none() {
            self.draw_auth_panel(frame, area, block);
            return;
        }

        let current_id = self.queue.current().map(|t| t.id.as_str());
        let inner_width = area.width.saturating_sub(4) as usize;

        let items: Vec<ListItem> = if let Some(alb) = &self.selected_album {
            alb.tracks
                .iter()
                .enumerate()
                .map(|(i, t)| {
                    format_track_item(
                        i + 1,
                        t,
                        current_id == Some(t.id.as_str()),
                        self.track_ratings.get(&t.id).copied(),
                        inner_width,
                    )
                })
                .collect()
        } else {
            match self.tab {
                Tab::Songs => {
                    if self.songs.is_empty() {
                        vec![ListItem::new(Line::from(Span::styled(
                            " > Press '/' to search songs or albums, Enter to play.",
                            Style::default().fg(AMBER_DIM),
                        )))]
                    } else {
                        self.songs
                            .iter()
                            .enumerate()
                            .map(|(i, t)| {
                                format_track_item(
                                    i + 1,
                                    t,
                                    current_id == Some(t.id.as_str()),
                                    self.track_ratings.get(&t.id).copied(),
                                    inner_width,
                                )
                            })
                            .collect()
                    }
                }
                Tab::Albums => {
                    if self.albums.is_empty() {
                        vec![ListItem::new(Line::from(Span::styled(
                            " > Search above to browse albums.",
                            Style::default().fg(AMBER_DIM),
                        )))]
                    } else {
                        self.albums
                            .iter()
                            .enumerate()
                            .map(|(i, a)| format_album_item(i + 1, a, inner_width))
                            .collect()
                    }
                }
                Tab::Library => {
                    if !self.auth_status.has_cookie {
                        vec![ListItem::new(Line::from(Span::styled(
                            " > Press '4' to link your Google account (DPAPI encrypted).",
                            Style::default().fg(AMBER_DIM),
                        )))]
                    } else if self.loading_library {
                        vec![ListItem::new(Line::from(Span::styled(
                            " > Loading your YouTube Music library...",
                            Style::default().fg(AMBER_DIM),
                        )))]
                    } else if self.library_section == LibrarySection::SavedAlbums {
                        self.library_albums
                            .iter()
                            .enumerate()
                            .map(|(i, a)| format_album_item(i + 1, a, inner_width))
                            .collect()
                    } else {
                        self.library_tracks
                            .iter()
                            .enumerate()
                            .map(|(i, t)| {
                                format_track_item(
                                    i + 1,
                                    t,
                                    current_id == Some(t.id.as_str()),
                                    self.track_ratings.get(&t.id).copied(),
                                    inner_width,
                                )
                            })
                            .collect()
                    }
                }
                Tab::Auth => Vec::new(),
            }
        };

        let list = List::new(items).block(block).highlight_style(
            Style::default()
                .bg(AMBER_MAIN)
                .fg(BG_OBSIDIAN)
                .add_modifier(Modifier::BOLD),
        );

        frame.render_stateful_widget(list, area, &mut self.catalog_state);
    }

    fn draw_auth_panel(&self, frame: &mut Frame, area: Rect, block: Block<'_>) {
        let vault = self.paths.data_dir.join(ENCRYPTED_SESSION_FILE);
        let browser_hint = self
            .detected_browser_name
            .as_deref()
            .unwrap_or("Not detected");
        let cookie_state = if self.session_expired {
            "EXPIRED (PRESS [g] TO RE-LOGIN)"
        } else if self.auth_status.has_cookie {
            "ACTIVE (PERSISTENT)"
        } else {
            "NONE"
        };

        let mut lines = vec![
            Line::from(Span::styled(
                format!(" VAULT: {} (WINDOWS DPAPI)", vault.display()),
                Style::default().fg(AMBER_BRIGHT),
            )),
            Line::from(Span::styled(
                format!(
                    " STATUS: GOOGLE_SESSION={}  //  OAUTH_TV={}",
                    cookie_state,
                    if self.auth_status.has_oauth { "ACTIVE" } else { "NONE" }
                ),
                Style::default().fg(if self.session_expired {
                    AMBER_ALERT
                } else {
                    AMBER_MAIN
                }),
            )),
            Line::from(""),
            Line::from(Span::styled(" COMMANDS:", Style::default().fg(AMBER_BRIGHT))),
            Line::from(Span::styled(
                format!("   [g] Sign in with Google ({browser_hint}) -> Auto-save in DPAPI"),
                Style::default()
                    .fg(AMBER_BRIGHT)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                "   [o] Link Google TV OAuth device code (opens google.com/device)",
                Style::default().fg(AMBER_MAIN),
            )),
            Line::from(Span::styled(
                "   [i] Import cookies.txt via Windows file dialog",
                Style::default().fg(AMBER_DIM),
            )),
            Line::from(Span::styled(
                "   [c] Paste Cookie header directly (masked + zeroized)",
                Style::default().fg(AMBER_DIM),
            )),
            Line::from(Span::styled(
                format!(
                    "   [t] Auto-wipe cookies.txt after import: [{}]",
                    if self.delete_cookie_file { "ON" } else { "OFF" }
                ),
                Style::default().fg(AMBER_DIM),
            )),
            Line::from(Span::styled(
                "   [x] Logout & clear stored session (cookies + OAuth)",
                Style::default().fg(AMBER_ALERT),
            )),
        ];

        if self.login_cancel.is_some() {
            lines.push(Line::from(Span::styled(
                "   [Esc] Cancel active browser sign-in",
                Style::default().fg(AMBER_ALERT),
            )));
        }

        if self.input_mode == InputMode::PasteCookie {
            let masked = "*".repeat(self.cookie_input.len().min(48));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                format!(" PASTE COOKIE (Enter=Save, Esc=Cancel) > {masked}_"),
                Style::default()
                    .bg(AMBER_MAIN)
                    .fg(BG_OBSIDIAN)
                    .add_modifier(Modifier::BOLD),
            )));
        }

        if let Some(upd) = &self.available_update {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                format!(
                    "  [u] NUEVA VERSIÓN DISPONIBLE: v{} (actual v{}) — Presiona [u] para actualizar",
                    upd.version, CURRENT_VERSION
                ),
                Style::default()
                    .bg(AMBER_MAIN)
                    .fg(BG_OBSIDIAN)
                    .add_modifier(Modifier::BOLD),
            )));
        }

        if let Some(fb) = &self.auth_feedback {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                format!(" > {fb}"),
                Style::default().fg(AMBER_BRIGHT),
            )));
        }

        let p = Paragraph::new(lines).block(block).wrap(Wrap { trim: false });
        frame.render_widget(p, area);
    }

    fn draw_queue(&mut self, frame: &mut Frame, area: Rect) {
        let auto_tag = if self.queue.autoplay_enabled {
            "AUTOPLAY: ON"
        } else {
            "AUTOPLAY: OFF"
        };
        let border_color = if self.focus == FocusPane::Queue {
            AMBER_MAIN
        } else {
            BRONZE
        };

        let title = format!(" [ UP NEXT // {auto_tag} ] ");
        let block = Block::default()
            .title(Span::styled(
                title,
                Style::default()
                    .fg(AMBER_BRIGHT)
                    .add_modifier(Modifier::BOLD),
            ))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color))
            .style(Style::default().bg(BG_PANEL));

        let inner_width = area.width.saturating_sub(4) as usize;
        let items: Vec<ListItem> = if self.queue.upcoming().is_empty() {
            let msg = if self.queue.is_radio_loading() {
                " > Syncing Google Radio..."
            } else {
                " > Play any track for Google Radio."
            };
            vec![ListItem::new(Line::from(Span::styled(
                msg,
                Style::default().fg(AMBER_DIM),
            )))]
        } else {
            self.queue
                .upcoming()
                .iter()
                .enumerate()
                .map(|(i, t)| {
                    format_track_item(
                        i + 1,
                        t,
                        false,
                        self.track_ratings.get(&t.id).copied(),
                        inner_width,
                    )
                })
                .collect()
        };

        let list = List::new(items).block(block).highlight_style(
            Style::default()
                .bg(AMBER_MAIN)
                .fg(BG_OBSIDIAN)
                .add_modifier(Modifier::BOLD),
        );

        frame.render_stateful_widget(list, area, &mut self.queue_state);
    }

    fn draw_player(&self, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(BRONZE))
            .style(Style::default().bg(BG_PANEL));

        let state_label = match self.playback_status {
            PlaybackStatus::Playing => "PLAYING",
            PlaybackStatus::Paused => "PAUSED",
            PlaybackStatus::Buffering => "BUFFERING",
            PlaybackStatus::Error(_) => "ERROR",
            PlaybackStatus::Idle => "IDLE",
        };

        let track_line = if let Some(t) = self.queue.current() {
            let codec = self.codec_label.as_deref().unwrap_or("AAC");
            let mut spans = vec![
                Span::styled(
                    format!(" {state_label}: {} ", t.title),
                    Style::default()
                        .fg(AMBER_BRIGHT)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("— {}  ", t.artist), Style::default().fg(AMBER_MAIN)),
            ];
            if self.auth_status.has_cookie {
                match self.track_ratings.get(&t.id).copied() {
                    Some(LikeStatus::Like) => spans.push(Span::styled(
                        "[♥ LIKED] ",
                        Style::default()
                            .fg(AMBER_BRIGHT)
                            .add_modifier(Modifier::BOLD),
                    )),
                    Some(LikeStatus::Dislike) => spans.push(Span::styled(
                        "[✕ DISLIKED] ",
                        Style::default()
                            .fg(AMBER_ALERT)
                            .add_modifier(Modifier::BOLD),
                    )),
                    Some(LikeStatus::Indifferent) => {
                        spans.push(Span::styled("[♡] ", Style::default().fg(BRONZE)));
                    }
                    None if self.rating_loading_for.as_deref() == Some(t.id.as_str()) => {
                        spans.push(Span::styled("[…] ", Style::default().fg(BRONZE)));
                    }
                    None => {}
                }
            }
            spans.push(Span::styled(
                format!("[{codec}]"),
                Style::default().fg(AMBER_DIM),
            ));
            Line::from(spans)
        } else {
            Line::from(Span::styled(
                " IDLE // NO TRACK LOADED",
                Style::default().fg(AMBER_DIM),
            ))
        };

        let total_s = self.duration.map(|d| d.as_secs()).unwrap_or(0);
        let cur_s = self.position.as_secs().min(total_s.max(self.position.as_secs()));
        let ratio = if total_s > 0 {
            (cur_s as f64 / total_s as f64).clamp(0.0, 1.0)
        } else {
            0.0
        };

        let bar_w = (area.width as usize).saturating_sub(54).clamp(12, 40);
        let filled = ((bar_w as f64) * ratio).round() as usize;
        let prog_bar = format!(
            "[{}{}]",
            "■".repeat(filled.min(bar_w)),
            "─".repeat(bar_w.saturating_sub(filled))
        );

        let vol_blocks = 8usize;
        let vol_filled = ((vol_blocks as f32) * self.settings.volume).round() as usize;
        let vol_bar = format!(
            "[{}{}] {:>3}%",
            "■".repeat(vol_filled.min(vol_blocks)),
            "─".repeat(vol_blocks.saturating_sub(vol_filled)),
            (self.settings.volume * 100.0).round() as u32
        );

        let bar_line = Line::from(vec![
            Span::styled(
                format!(" {} ", format_secs(cur_s as u32)),
                Style::default().fg(AMBER_BRIGHT),
            ),
            Span::styled(prog_bar, Style::default().fg(AMBER_MAIN)),
            Span::styled(
                format!(
                    " {}   ",
                    if total_s > 0 {
                        format_secs(total_s as u32)
                    } else {
                        "--:--".to_string()
                    }
                ),
                Style::default().fg(AMBER_DIM),
            ),
            Span::styled("VOL ", Style::default().fg(AMBER_DIM)),
            Span::styled(vol_bar, Style::default().fg(AMBER_MAIN)),
        ]);

        let help_line = Line::from(Span::styled(
            " [/]Find [1-4]Tabs [Tab]Pane [Enter]Play [Space]Pause [n/p]Next/Prev [←/→]Seek [+/-]Vol [f/L]Like [D]Dislike [a]Autoplay [e]Queue [q]Quit",
            Style::default().fg(BRONZE),
        ));

        let p = Paragraph::new(vec![track_line, bar_line, help_line]).block(block);
        frame.render_widget(p, area);
    }
}

fn format_track_item<'a>(
    index: usize,
    track: &Track,
    is_playing: bool,
    rating: Option<LikeStatus>,
    width: usize,
) -> ListItem<'a> {
    let dur = track
        .duration_secs
        .map(format_secs)
        .unwrap_or_else(|| "--:--".to_string());
    let prefix = if is_playing {
        format!("▶ {:02} ", index)
    } else {
        format!("  {:02} ", index)
    };
    let marker = match rating {
        Some(LikeStatus::Like) => "♥ ",
        Some(LikeStatus::Dislike) => "✕ ",
        _ => "",
    };
    let label = format!("{} — {}", track.title, track.artist);
    let fixed_chars = prefix.chars().count() + marker.chars().count() + dur.chars().count() + 2;
    let max_label = width.saturating_sub(fixed_chars).max(10);
    let truncated = truncate_str(&label, max_label);
    let pad = max_label.saturating_sub(truncated.chars().count());
    let line_str = format!("{prefix}{marker}{truncated}{} {dur}", " ".repeat(pad));

    let style = if is_playing {
        Style::default()
            .fg(AMBER_BRIGHT)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(AMBER_MAIN)
    };
    ListItem::new(Line::from(Span::styled(line_str, style)))
}

fn format_album_item<'a>(index: usize, album: &AlbumSummary, width: usize) -> ListItem<'a> {
    let yr = album
        .year
        .map(|y| y.to_string())
        .unwrap_or_else(|| "----".to_string());
    let label = format!("  {:02} {} — {} ({})", index, album.title, album.artist, yr);
    let truncated = truncate_str(&label, width.max(12));
    ListItem::new(Line::from(Span::styled(
        truncated,
        Style::default().fg(AMBER_MAIN),
    )))
}

fn truncate_str(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        s.to_string()
    } else if max_chars > 1 {
        let mut out: String = s.chars().take(max_chars - 1).collect();
        out.push('…');
        out
    } else {
        s.chars().take(max_chars).collect()
    }
}

fn format_secs(total: u32) -> String {
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

/// Ejecuta la interfaz completa de terminal (TUI Amber Monochrome Console).
pub async fn run_tui(
    paths: AppPaths,
    settings: AppSettings,
    ytm: YtmClient,
    audio: AudioEngine,
) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let _guard = TerminalGuard;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let (tx, mut rx) = mpsc::unbounded_channel::<AsyncEvent>();
    let (media_tx, mut media_rx) = mpsc::unbounded_channel::<OsMediaEvent>();
    let media_keys = MediaKeysHandle::spawn(media_tx);
    let mut app = TuiApp::new(paths, settings, ytm, audio, media_keys, tx);

    let mut last_tick = Instant::now();

    loop {
        terminal.draw(|f| app.draw(f))?;

        // Procesamos todos los eventos async y teclas multimedia del SO sin esperar
        while let Ok(ev) = rx.try_recv() {
            app.handle_async(ev);
        }
        while let Ok(mev) = media_rx.try_recv() {
            app.handle_os_media(mev);
        }

        // Poll corto de teclado/mouse (60ms cuando reproduce, 150ms en reposo -> CPU mínima)
        let timeout = if matches!(
            app.playback_status,
            PlaybackStatus::Playing | PlaybackStatus::Buffering
        ) {
            Duration::from_millis(60)
        } else {
            Duration::from_millis(150)
        };

        if event::poll(timeout)? {
            match event::read()? {
                Event::Key(k) => app.handle_key(k),
                Event::Mouse(m) => match m.kind {
                    MouseEventKind::ScrollDown => app.move_selection(1),
                    MouseEventKind::ScrollUp => app.move_selection(-1),
                    _ => {}
                },
                _ => {}
            }
        }

        if last_tick.elapsed() >= Duration::from_millis(200) {
            last_tick = Instant::now();
            app.tick_audio();
        }

        if app.should_quit {
            app.cancel_browser_login();
            app.audio.stop();
            break;
        }
    }

    Ok(())
}
