//! Spotify Web API objects (as JSON) → `listen_core::model`. Read leniently: Spotify has been
//! dropping fields (popularity, followers, labels) from the responses to development-mode apps, so
//! nothing here requires more than an id and a name.

use listen_core::model::{
    Album, AlbumInfo, Artist, ArtistRef, GainInfo, ImageRef, Playlist, ServerId, Song, SongExtra, SourceKind,
};
use serde_json::Value;
use std::time::Duration;

pub fn str_of(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string)
}

fn u64_of(v: &Value, key: &str) -> Option<u64> {
    v.get(key).and_then(Value::as_u64)
}

/// The largest image of an `images` array.
pub fn image(v: &Value) -> Option<ImageRef> {
    let images = v.get("images")?.as_array()?;
    images
        .iter()
        .filter_map(|i| Some((i.get("width").and_then(Value::as_u64).unwrap_or(0), str_of(i, "url")?)))
        .max_by_key(|(w, _)| *w)
        .map(|(_, url)| ImageRef::Url(url))
}

pub fn artist_refs(v: &Value) -> Vec<ArtistRef> {
    v.get("artists")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|a| Some(ArtistRef { id: str_of(a, "id"), name: str_of(a, "name")? }))
                .collect()
        })
        .unwrap_or_default()
}

/// "2011-03-14" / "2011-03" / "2011" → 2011.
pub fn year(date: &str) -> Option<i32> {
    date.get(..4)?.parse().ok()
}

/// The item of a playlist / library entry (`{"added_at": …, "track": {…}}`; newer responses call
/// it `item`). Missing for tracks that were removed from Spotify.
pub fn entry_item(v: &Value) -> Option<&Value> {
    v.get("track").or_else(|| v.get("item")).filter(|t| t.is_object())
}

/// A track (full or simplified). `album` is the album a simplified track (album page) belongs to.
/// `None` for podcast episodes, local files and tracks without an id.
pub fn track(v: &Value, server_id: &ServerId, album: Option<&Value>) -> Option<Song> {
    if v.get("type").and_then(Value::as_str).is_some_and(|t| t != "track") {
        return None;
    }
    if v.get("is_local").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let id = str_of(v, "id")?;
    let album = v.get("album").filter(|a| a.is_object()).or(album);
    let release_date = album.and_then(|a| str_of(a, "release_date"));
    let artists = artist_refs(v);
    let album_artists = album.map(artist_refs).filter(|a| !a.is_empty()).unwrap_or_else(|| artists.clone());
    Some(Song {
        id,
        server_id: server_id.clone(),
        source: SourceKind::Spotify,
        title: str_of(v, "name").unwrap_or_default(),
        artists,
        album_artists,
        album: album.and_then(|a| str_of(a, "name")),
        album_id: album.and_then(|a| str_of(a, "id")),
        image: album.and_then(image),
        duration: Duration::from_millis(u64_of(v, "duration_ms").unwrap_or(0)),
        track_number: u64_of(v, "track_number").map(|n| n as u32),
        disc_number: u64_of(v, "disc_number").map(|n| n as u32),
        year: release_date.as_deref().and_then(year),
        genres: vec![],
        codec: None,
        bit_rate: None,
        sample_rate: None,
        bit_depth: None,
        bpm: None,
        favorite: false,
        rating: None,
        play_count: None,
        gain: GainInfo::default(),
        explicit: v.get("explicit").and_then(Value::as_bool).unwrap_or(false),
        path: None,
        size: None,
        suffix: None,
        channels: None,
        created: None,
        comment: None,
        mbz: Default::default(),
        extra: SongExtra { release_date, ..Default::default() },
    })
}

/// An album; `songs` filled from an embedded `tracks` page (the album endpoint).
pub fn album(v: &Value, server_id: &ServerId) -> Option<Album> {
    let id = str_of(v, "id")?;
    let release_date = str_of(v, "release_date");
    let release_type = str_of(v, "album_type");
    let songs: Vec<Song> = v
        .pointer("/tracks/items")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(|t| track(t, server_id, Some(v))).collect())
        .unwrap_or_default();
    let duration = songs.iter().map(|s| s.duration).sum();
    let genres = v
        .get("genres")
        .and_then(Value::as_array)
        .map(|g| g.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default();
    Some(Album {
        id,
        server_id: server_id.clone(),
        source: SourceKind::Spotify,
        name: str_of(v, "name").unwrap_or_default(),
        album_artists: artist_refs(v),
        image: image(v),
        year: release_date.as_deref().and_then(year),
        release_types: release_type.iter().cloned().collect(),
        release_type,
        genres,
        song_count: u64_of(v, "total_tracks").map(|n| n as u32),
        duration,
        favorite: false,
        rating: None,
        play_count: None,
        songs,
        info: AlbumInfo {
            release_date,
            explicit: false,
            record_labels: str_of(v, "label").into_iter().collect(),
            ..Default::default()
        },
    })
}

pub fn artist(v: &Value, server_id: &ServerId) -> Option<Artist> {
    Some(Artist {
        id: str_of(v, "id")?,
        server_id: server_id.clone(),
        source: SourceKind::Spotify,
        name: str_of(v, "name").unwrap_or_default(),
        image: image(v),
        album_count: None,
        song_count: None,
        favorite: false,
        rating: None,
        biography: None,
        similar: vec![],
    })
}

/// A playlist; `me` is the signed-in user's id (their own and collaborative playlists are editable).
pub fn playlist(v: &Value, server_id: &ServerId, me: &str) -> Option<Playlist> {
    let owner_id = v.pointer("/owner/id").and_then(Value::as_str).unwrap_or_default();
    let owner = v.get("owner").and_then(|o| str_of(o, "display_name").or_else(|| str_of(o, "id")));
    let collaborative = v.get("collaborative").and_then(Value::as_bool).unwrap_or(false);
    let count = v
        .pointer("/tracks/total")
        .or_else(|| v.pointer("/items/total"))
        .and_then(Value::as_u64)
        .map(|n| n as u32);
    Some(Playlist {
        id: str_of(v, "id")?,
        server_id: server_id.clone(),
        source: SourceKind::Spotify,
        name: str_of(v, "name").unwrap_or_default(),
        description: str_of(v, "description").map(|d| strip_tags(&d)).filter(|d| !d.is_empty()),
        owner,
        image: image(v),
        song_count: count,
        duration: Duration::ZERO,
        public: v.get("public").and_then(Value::as_bool).unwrap_or(false),
        editable: collaborative || (!me.is_empty() && owner_id == me),
    })
}

/// Spotify descriptions carry HTML links and entities.
pub fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sid() -> ServerId {
        ServerId("sp".into())
    }

    fn full_track() -> Value {
        json!({
            "type": "track", "id": "4uLU6hMCjMI75M1A2tKUQC", "name": "Never Gonna Give You Up",
            "duration_ms": 213573, "explicit": false, "track_number": 1, "disc_number": 1,
            "artists": [{"id": "0gxyHStUsqpMadRV0Di1Qt", "name": "Rick Astley"}],
            "album": {
                "id": "6N9PS4QXF1D0OWPk0Sxtb4", "name": "Whenever You Need Somebody",
                "release_date": "1987-11-12", "album_type": "album",
                "artists": [{"id": "0gxyHStUsqpMadRV0Di1Qt", "name": "Rick Astley"}],
                "images": [
                    {"url": "https://i.scdn.co/image/small", "width": 64, "height": 64},
                    {"url": "https://i.scdn.co/image/large", "width": 640, "height": 640}
                ]
            }
        })
    }

    #[test]
    fn reads_a_full_track() {
        let s = track(&full_track(), &sid(), None).unwrap();
        assert_eq!(s.id, "4uLU6hMCjMI75M1A2tKUQC");
        assert_eq!(s.source, SourceKind::Spotify);
        assert_eq!(s.title, "Never Gonna Give You Up");
        assert_eq!(s.artist_line(), "Rick Astley");
        assert_eq!(s.album.as_deref(), Some("Whenever You Need Somebody"));
        assert_eq!(s.album_id.as_deref(), Some("6N9PS4QXF1D0OWPk0Sxtb4"));
        assert_eq!(s.image, Some(ImageRef::Url("https://i.scdn.co/image/large".into())));
        assert_eq!(s.duration, Duration::from_millis(213573));
        assert_eq!(s.year, Some(1987));
        assert_eq!(s.track_number, Some(1));
    }

    #[test]
    fn skips_episodes_local_files_and_removed_tracks() {
        assert!(track(&json!({"type": "episode", "id": "e", "name": "Pod"}), &sid(), None).is_none());
        assert!(track(&json!({"id": null, "name": "Local", "is_local": true}), &sid(), None).is_none());
        assert!(entry_item(&json!({"added_at": "x", "track": null})).is_none());
        assert!(entry_item(&json!({"item": full_track()})).is_some());
    }

    #[test]
    fn album_tracks_inherit_the_album() {
        let v = json!({
            "id": "a1", "name": "Album", "release_date": "2020", "album_type": "single", "total_tracks": 2,
            "label": "Label", "artists": [{"id": "x", "name": "X"}], "images": [],
            "tracks": {"items": [
                {"id": "t1", "name": "One", "duration_ms": 1000, "artists": [{"id": "x", "name": "X"}]},
                {"id": "t2", "name": "Two", "duration_ms": 2000, "artists": [{"id": "y", "name": "Y"}]}
            ]}
        });
        let a = album(&v, &sid()).unwrap();
        assert_eq!(a.songs.len(), 2);
        assert_eq!(a.songs[1].album_id.as_deref(), Some("a1"));
        assert_eq!(a.songs[1].album_artists[0].name, "X");
        assert_eq!(a.duration, Duration::from_secs(3));
        assert_eq!(a.release_type.as_deref(), Some("single"));
        assert_eq!(a.year, Some(2020));
        assert_eq!(a.info.record_labels, vec!["Label".to_string()]);
    }

    #[test]
    fn playlists_of_the_user_are_editable() {
        let v = json!({
            "id": "p1", "name": "Mine", "description": "Songs &amp; <a href=\"x\">more</a>",
            "owner": {"id": "me", "display_name": "Me"}, "public": true, "collaborative": false,
            "tracks": {"total": 12}, "images": null
        });
        let p = playlist(&v, &sid(), "me").unwrap();
        assert!(p.editable);
        assert_eq!(p.song_count, Some(12));
        assert_eq!(p.description.as_deref(), Some("Songs & more"));
        assert_eq!(p.owner.as_deref(), Some("Me"));
        assert!(p.image.is_none());
        assert!(!playlist(&v, &sid(), "someone-else").unwrap().editable);
    }
}
