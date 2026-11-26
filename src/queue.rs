use std::collections::{HashSet, VecDeque};

use crate::ytm::{RadioPage, Track};

const MAX_HISTORY: usize = 100;
const RADIO_REFILL_THRESHOLD: usize = 3;

/// Semilla usada para solicitar más recomendaciones de Google cuando la cola se vacía.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RadioSeed {
    /// Radio basada en una canción (RDAMVM<video_id>).
    Track(String),
    /// Radio basada en una playlist/álbum (RDAMPL<playlist_id>).
    Playlist(String),
}

/// Estado de la cola de reproducción y del motor de autoplay (Radio infinita).
#[derive(Debug, Clone)]
pub struct PlaybackQueue {
    history: Vec<Track>,
    current: Option<Track>,
    upcoming: VecDeque<Track>,
    recent_ids: VecDeque<String>,
    recent_set: HashSet<String>,

    pub autoplay_enabled: bool,
    radio_seed: Option<RadioSeed>,
    radio_continuation: Option<String>,
    radio_visitor_data: Option<String>,
    radio_loading: bool,
}

/// Qué acción de red debería disparar el controlador tras un cambio de cola.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RadioFetchRequest {
    None,
    Initial(RadioSeed),
    Continuation {
        token: String,
        visitor_data: Option<String>,
        fallback_seed: Option<RadioSeed>,
    },
}

impl PlaybackQueue {
    pub fn new(autoplay_enabled: bool) -> Self {
        Self {
            history: Vec::new(),
            current: None,
            upcoming: VecDeque::new(),
            recent_ids: VecDeque::new(),
            recent_set: HashSet::new(),
            autoplay_enabled,
            radio_seed: None,
            radio_continuation: None,
            radio_visitor_data: None,
            radio_loading: false,
        }
    }

    pub fn current(&self) -> Option<&Track> {
        self.current.as_ref()
    }

    pub fn upcoming(&self) -> &VecDeque<Track> {
        &self.upcoming
    }

    pub fn can_go_previous(&self) -> bool {
        !self.history.is_empty()
    }

    pub fn is_radio_loading(&self) -> bool {
        self.radio_loading
    }

    pub fn set_radio_loading(&mut self, loading: bool) {
        self.radio_loading = loading;
    }

    /// Reproduce una canción individual y prepara la radio de Google a partir de ella.
    pub fn play_single_track(&mut self, track: Track) -> RadioFetchRequest {
        self.push_current_to_history();
        let seed_id = track.id.clone();
        self.remember_id(&seed_id);
        self.current = Some(track);
        self.upcoming.clear();
        self.radio_continuation = None;
        self.radio_visitor_data = None;
        self.radio_seed = Some(RadioSeed::Track(seed_id.clone()));

        if self.autoplay_enabled {
            self.radio_loading = true;
            RadioFetchRequest::Initial(RadioSeed::Track(seed_id))
        } else {
            self.radio_loading = false;
            RadioFetchRequest::None
        }
    }

    /// Reproduce una lista de pistas (por ejemplo, un álbum completo) desde `start_index`.
    /// Si `playlist_id` está disponible, lo guarda como semilla de radio para cuando termine el álbum.
    pub fn play_track_list(
        &mut self,
        tracks: Vec<Track>,
        start_index: usize,
        playlist_id: Option<String>,
    ) -> RadioFetchRequest {
        if tracks.is_empty() {
            return RadioFetchRequest::None;
        }
        let idx = start_index.min(tracks.len() - 1);
        self.push_current_to_history();
        self.upcoming.clear();
        self.radio_continuation = None;
        self.radio_visitor_data = None;

        let mut iter = tracks.into_iter().skip(idx);
        let first = match iter.next() {
            Some(t) => t,
            None => return RadioFetchRequest::None,
        };
        let first_id = first.id.clone();
        self.remember_id(&first_id);
        self.current = Some(first);

        let mut last_id = first_id;
        for t in iter {
            last_id = t.id.clone();
            self.upcoming.push_back(t);
        }

        self.radio_seed = Some(match playlist_id {
            Some(pid) => RadioSeed::Playlist(pid),
            None => RadioSeed::Track(last_id),
        });

        self.check_refill_needed()
    }

    /// Agrega una canción al final de la cola manual.
    pub fn enqueue_track(&mut self, track: Track) {
        self.upcoming.push_back(track);
    }

    /// Quita una canción de la cola por índice.
    pub fn remove_upcoming(&mut self, index: usize) {
        if index < self.upcoming.len() {
            self.upcoming.remove(index);
        }
    }

    /// Salta directamente a una canción específica de la cola "A continuación".
    pub fn jump_to_upcoming(&mut self, index: usize) -> Option<(Track, RadioFetchRequest)> {
        if index >= self.upcoming.len() {
            return None;
        }
        self.push_current_to_history();
        // Descarta las pistas intermedias anteriores al índice elegido
        for _ in 0..index {
            let _ = self.upcoming.pop_front();
        }
        let chosen = self.upcoming.pop_front()?;
        self.remember_id(&chosen.id);
        self.current = Some(chosen.clone());
        let fetch = self.check_refill_needed();
        Some((chosen, fetch))
    }

    /// Avanza a la siguiente canción en cola (automático al terminar o botón Siguiente).
    pub fn advance_next(&mut self) -> Option<(Track, RadioFetchRequest)> {
        let next = self.upcoming.pop_front()?;
        self.push_current_to_history();
        self.remember_id(&next.id);
        self.current = Some(next.clone());

        // Si la cola se agotó y no hay token de continuación, re-sembramos desde la pista actual.
        if self.upcoming.is_empty() && self.radio_continuation.is_none() {
            self.radio_seed = Some(RadioSeed::Track(next.id.clone()));
        }

        let fetch = self.check_refill_needed();
        Some((next, fetch))
    }

    /// Vuelve a la pista anterior del historial.
    pub fn go_previous(&mut self) -> Option<Track> {
        let prev = self.history.pop()?;
        if let Some(curr) = self.current.take() {
            self.upcoming.push_front(curr);
        }
        self.current = Some(prev.clone());
        Some(prev)
    }

    /// Incorpora una página de recomendaciones devuelta por YouTube Music (deduplicando).
    pub fn apply_radio_page(&mut self, page: RadioPage) -> usize {
        self.radio_loading = false;
        self.radio_continuation = page.continuation;
        if page.visitor_data.is_some() {
            self.radio_visitor_data = page.visitor_data;
        }

        let upcoming_ids: HashSet<String> = self.upcoming.iter().map(|t| t.id.clone()).collect();
        let current_id = self.current.as_ref().map(|t| t.id.as_str());

        let mut added = 0usize;
        let mut last_added_id: Option<String> = None;

        for track in page.tracks {
            if current_id == Some(track.id.as_str()) {
                continue;
            }
            if upcoming_ids.contains(&track.id) || self.recent_set.contains(&track.id) {
                continue;
            }
            last_added_id = Some(track.id.clone());
            self.upcoming.push_back(track);
            added += 1;
        }

        // Actualiza la semilla de respaldo con la última pista recibida para radio infinita
        if let Some(id) = last_added_id {
            self.radio_seed = Some(RadioSeed::Track(id));
        }

        added
    }

    /// Evalúa si debemos pedir más canciones recomendadas a Google.
    pub fn check_refill_needed(&mut self) -> RadioFetchRequest {
        if !self.autoplay_enabled || self.radio_loading {
            return RadioFetchRequest::None;
        }
        if self.upcoming.len() > RADIO_REFILL_THRESHOLD {
            return RadioFetchRequest::None;
        }

        if let Some(token) = self.radio_continuation.take() {
            self.radio_loading = true;
            return RadioFetchRequest::Continuation {
                token,
                visitor_data: self.radio_visitor_data.clone(),
                fallback_seed: self
                    .radio_seed
                    .clone()
                    .or_else(|| self.current.as_ref().map(|t| RadioSeed::Track(t.id.clone()))),
            };
        }

        let seed = self
            .radio_seed
            .clone()
            .or_else(|| self.current.as_ref().map(|t| RadioSeed::Track(t.id.clone())));

        if let Some(seed) = seed {
            self.radio_loading = true;
            RadioFetchRequest::Initial(seed)
        } else {
            RadioFetchRequest::None
        }
    }

    /// Cambia el estado de Autoplay; si se activa y hay pocas canciones, dispara recarga.
    pub fn set_autoplay(&mut self, enabled: bool) -> RadioFetchRequest {
        self.autoplay_enabled = enabled;
        if enabled {
            self.check_refill_needed()
        } else {
            RadioFetchRequest::None
        }
    }

    fn push_current_to_history(&mut self) {
        if let Some(curr) = self.current.take() {
            self.history.push(curr);
            if self.history.len() > MAX_HISTORY {
                self.history.remove(0);
            }
        }
    }

    fn remember_id(&mut self, id: &str) {
        if self.recent_set.insert(id.to_string()) {
            self.recent_ids.push_back(id.to_string());
            if self.recent_ids.len() > MAX_HISTORY {
                if let Some(old) = self.recent_ids.pop_front() {
                    self.recent_set.remove(&old);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_track(id: &str) -> Track {
        Track {
            id: id.to_string(),
            title: format!("Song {id}"),
            artist: "Artist".to_string(),
            album: None,
            duration_secs: Some(200),
            cover_url: None,
        }
    }

    #[test]
    fn single_track_triggers_radio_and_deduplicates() {
        let mut q = PlaybackQueue::new(true);
        let req = q.play_single_track(mk_track("a1"));
        assert_eq!(req, RadioFetchRequest::Initial(RadioSeed::Track("a1".into())));
        assert!(q.is_radio_loading());

        let added = q.apply_radio_page(RadioPage {
            tracks: vec![mk_track("a1"), mk_track("b2"), mk_track("c3"), mk_track("d4")],
            continuation: Some("tok_1".into()),
            visitor_data: None,
        });
        assert_eq!(added, 3);
        assert!(!q.is_radio_loading());
        assert_eq!(q.upcoming().len(), 3);

        // Al avanzar a b2 quedan 2 en cola (<= 3) => pide continuación automáticamente
        let (next, fetch) = q.advance_next().unwrap();
        assert_eq!(next.id, "b2");
        assert!(matches!(
            fetch,
            RadioFetchRequest::Continuation { ref token, .. } if token == "tok_1"
        ));
    }

    #[test]
    fn album_plays_all_tracks_then_refills_radio_near_end() {
        let mut q = PlaybackQueue::new(true);
        let album_tracks = (1..=6).map(|i| mk_track(&format!("t{i}"))).collect();

        // Quedan 5 en cola (> 3), no pide radio todavía
        let req = q.play_track_list(album_tracks, 0, Some("OLAK_123".into()));
        assert_eq!(req, RadioFetchRequest::None);
        assert_eq!(q.current().unwrap().id, "t1");

        // Avanza a t2 (quedan 4) -> todavía no
        let (_, req) = q.advance_next().unwrap();
        assert_eq!(req, RadioFetchRequest::None);

        // Avanza a t3 (quedan 3 <= 3) -> dispara radio del álbum
        let (t, req) = q.advance_next().unwrap();
        assert_eq!(t.id, "t3");
        assert_eq!(
            req,
            RadioFetchRequest::Initial(RadioSeed::Playlist("OLAK_123".into()))
        );
    }

    #[test]
    fn previous_restores_current_track() {
        let mut q = PlaybackQueue::new(false);
        q.play_track_list(vec![mk_track("t1"), mk_track("t2")], 0, None);
        q.advance_next();
        assert_eq!(q.current().unwrap().id, "t2");

        let prev = q.go_previous().unwrap();
        assert_eq!(prev.id, "t1");
        assert_eq!(q.upcoming().front().unwrap().id, "t2");
    }
}
