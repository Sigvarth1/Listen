//! Looks up the YouTube Music copies of a few well-known Spotify tracks (no Spotify sign-in needed)
//! and resolves the first one's stream: `cargo run -p listen-spotify --example spotify_match_check`.

use listen_core::model::{ArtistRef, ServerId, Song, SourceKind};
use listen_source::MusicSource;
use std::time::Duration;

fn track(title: &str, artist: &str, secs: u64) -> Song {
    let mut s: Song = serde_json::from_value(serde_json::json!({
        "id": title, "server_id": "sp", "source": "spotify", "title": title, "artists": [],
        "album_artists": [], "album": null, "album_id": null, "image": null, "duration": 0.0,
        "track_number": null, "disc_number": null, "year": null, "genres": [], "codec": null,
        "bit_rate": null, "sample_rate": null, "bit_depth": null, "bpm": null, "favorite": false,
        "rating": null, "play_count": null, "gain": {"track_gain": null, "track_peak": null,
        "album_gain": null, "album_peak": null}, "explicit": false
    }))
    .unwrap();
    s.artists = vec![ArtistRef { id: None, name: artist.into() }];
    s.duration = Duration::from_secs(secs);
    s.source = SourceKind::Spotify;
    s
}

#[tokio::main]
async fn main() {
    let yt = listen_ytmusic::YouTubeMusic::connect(ServerId("yt".into()), listen_ytmusic::GUEST_COOKIE)
        .await
        .expect("YouTube Music");
    let wanted = [
        track("Bohemian Rhapsody - Remastered 2011", "Queen", 354),
        track("Here Comes The Sun - Remastered 2009", "The Beatles", 185),
        track("Blinding Lights", "The Weeknd", 200),
        track("Stay (with Justin Bieber)", "The Kid LAROI", 141),
        track("Dancing Queen", "ABBA", 231),
        track("Take On Me", "a-ha", 225),
    ];
    let mut first = None;
    for w in &wanted {
        let candidates = yt.search_songs(&listen_spotify::matching::query(w)).await.unwrap_or_default();
        match listen_spotify::matching::best(w, &candidates) {
            Some(m) => {
                println!(
                    "{:<40} → {} — {} [{}] ({}s, score {:.2})",
                    w.title,
                    m.artist_line(),
                    m.title,
                    m.id,
                    m.duration.as_secs(),
                    listen_spotify::matching::score(w, m)
                );
                first.get_or_insert(m.clone());
            }
            None => println!("{:<40} → no match among {} results", w.title, candidates.len()),
        }
    }
    if let Some(m) = first {
        match yt.stream(&m).await {
            Ok(s) => println!("stream: {}…", &s.url[..s.url.len().min(80)]),
            Err(e) => println!("stream failed: {e}"),
        }
    }
}
