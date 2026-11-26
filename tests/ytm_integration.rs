use yt_niusic::audio::prepare_decoded_stream;
use yt_niusic::ytm::YtmClient;

/// Prueba real contra YouTube Music (opt-in con `--ignored --nocapture`).
/// Verifica:
/// 1. Búsqueda de canciones y álbumes
/// 2. Carga de pistas de un álbum
/// 3. Recomendaciones automáticas de Google (Radio) + paginación de continuación
/// 4. Resolución del stream, descarga por bloques Range (`ChunkedYtStream`) y decodificación PCM real
#[tokio::test]
#[ignore]
async fn ytm_live_search_radio_and_stream() {
    let tmp = std::env::temp_dir().join("yt_niusic_integration_test");
    let _ = std::fs::create_dir_all(&tmp);
    let client = YtmClient::new(&tmp).expect("init YtmClient");

    let songs = client
        .search_songs("Daft Punk Instant Crush")
        .await
        .expect("search_songs");
    assert!(!songs.is_empty(), "should return songs");
    let first_song = &songs[0];
    println!(
        "Song: {} - {} ({})",
        first_song.title, first_song.artist, first_song.id
    );

    let albums = client
        .search_albums("Random Access Memories")
        .await
        .expect("search_albums");
    assert!(!albums.is_empty(), "should return albums");
    let first_album = &albums[0];
    println!(
        "Album: {} - {} ({})",
        first_album.title, first_album.artist, first_album.id
    );

    let detail = client
        .get_album(&first_album.id)
        .await
        .expect("get_album");
    assert!(!detail.tracks.is_empty(), "album should contain tracks");
    println!("Album tracks: {}", detail.tracks.len());

    let radio = client
        .get_radio_for_track(&first_song.id)
        .await
        .expect("get_radio_for_track");
    assert!(
        !radio.tracks.is_empty(),
        "Google radio should return recommended tracks"
    );
    println!(
        "Radio recommendations: {} (has continuation: {})",
        radio.tracks.len(),
        radio.continuation.is_some()
    );

    if let Some(ctoken) = &radio.continuation {
        let next_page = client
            .get_radio_continuation(ctoken, radio.visitor_data.as_deref())
            .await
            .expect("get_radio_continuation");
        println!(
            "Radio continuation page: {} tracks",
            next_page.tracks.len()
        );
        assert!(
            !next_page.tracks.is_empty(),
            "continuation should return more recommended tracks"
        );
    }

    let stream = client
        .resolve_audio_stream(&first_song.id)
        .await
        .expect("resolve_audio_stream");
    println!(
        "Resolved stream: {} ({} kbps, size={:?})",
        stream.codec_label, stream.bitrate_kbps, stream.content_length
    );
    assert!(stream.url.starts_with("https://"));

    let (mut decoder, total_dur) = prepare_decoded_stream(&stream)
        .await
        .expect("prepare_decoded_stream via ChunkedYtStream");
    println!("Decoded duration: {total_dur:?}");

    let decoded_samples = tokio::task::spawn_blocking(move || {
        let mut count = 0usize;
        for _ in 0..4096 {
            if decoder.next().is_some() {
                count += 1;
            } else {
                break;
            }
        }
        count
    })
    .await
    .expect("join decode task");

    println!("Decoded PCM samples verified: {decoded_samples}");
    assert_eq!(decoded_samples, 4096, "should decode live AAC audio frames");

    let _ = std::fs::remove_dir_all(&tmp);
}
