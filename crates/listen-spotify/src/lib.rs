//! Spotify backend: the user's library, playlists, search, albums and artists through the Spotify
//! Web API ([`auth`] signs in with PKCE). With Spotify Premium and a player sign-in, songs play from
//! Spotify itself ([`player`], as librespot does); otherwise, or when Spotify can't play a song, from
//! its YouTube Music copy ([`matching`]) through the same yt-dlp path as the YouTube Music source.

pub mod auth;
mod convert;
pub mod matching;
pub mod player;

use async_trait::async_trait;
use futures::StreamExt;
use listen_core::model::{
    Album, Artist, ImageRef, Page, Playlist, SearchResults, ServerId, Song, SourceKind,
};
use listen_core::settings::StreamQuality;
use listen_source::{
    AlbumQuery, ArtistQuery, Capabilities, HomeShelf, ItemKind, ListQuery, MusicSource, PlaylistUpdate,
    Result, SongQuery, SourceError, StreamSource, clientside,
};
use listen_ytmusic::YouTubeMusic;
use reqwest::Method;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

const API: &str = "https://api.spotify.com/v1";
/// Library lists are fetched completely and reused for this long.
const LIBRARY_TTL: Duration = Duration::from_secs(10 * 60);
/// Id of the "Liked Songs" pseudo playlist.
pub const LIKED_PLAYLIST: &str = "liked";
/// Songs per page of a playlist.
const PLAYLIST_PAGE: usize = 100;
/// Parallel requests while reading a long list.
const PARALLEL: usize = 4;

type Saver = dyn Fn(&ServerId, &str) + Send + Sync;
static SAVER: OnceLock<Box<Saver>> = OnceLock::new();

/// Where a refresh token that Spotify replaced is stored (the app's keyring), so the next start
/// signs in with the new one. Set once at start-up.
pub fn set_secret_saver(save: impl Fn(&ServerId, &str) + Send + Sync + 'static) {
    let _ = SAVER.set(Box::new(save));
}

pub fn capabilities() -> Capabilities {
    Capabilities::PLAYLIST_EDIT
        | Capabilities::PLAYLIST_REMOVE
        | Capabilities::PLAYLIST_RENAME
        | Capabilities::PLAYLIST_REORDER
        | Capabilities::PLAYLIST_UPDATE
        | Capabilities::FAVORITE_ITEMS
        | Capabilities::HOME_FEED
}

struct TokenState {
    access: String,
    refresh: String,
    expires: Instant,
}

struct Cached<T> {
    at: Instant,
    items: T,
}

impl<T: Clone> Cached<T> {
    fn fresh(slot: &Option<Self>) -> Option<T> {
        slot.as_ref().filter(|c| c.at.elapsed() < LIBRARY_TTL).map(|c| c.items.clone())
    }
}

/// The signed-in account (`GET /me`).
#[derive(Debug, Clone, Default)]
pub struct Me {
    pub id: String,
    pub display_name: String,
}

pub struct Spotify {
    server_id: ServerId,
    client_id: String,
    http: reqwest::Client,
    token: Mutex<TokenState>,
    me: std::sync::Mutex<Me>,
    liked: Mutex<Option<Cached<Arc<clientside::SongList>>>>,
    /// Ids of the liked songs, to mark songs in other lists (only once the library was read).
    liked_ids: std::sync::Mutex<Option<HashSet<String>>>,
    albums: Mutex<Option<Cached<Vec<Album>>>>,
    artists: Mutex<Option<Cached<Vec<Artist>>>>,
    /// YouTube Music (signed out) for the audio.
    yt: tokio::sync::OnceCell<Arc<YouTubeMusic>>,
    /// Spotify track id → its YouTube Music copy.
    matches: std::sync::Mutex<HashMap<String, Song>>,
    quality: std::sync::atomic::AtomicU8,
    /// Playback from Spotify itself (Premium and a player sign-in).
    player: Option<Arc<player::Player>>,
    /// Songs Spotify failed to play: they play from YouTube Music from then on.
    youtube_only: std::sync::Mutex<HashSet<String>>,
    /// What is stored (kept current when Spotify replaces a token or the player login).
    stored: Arc<std::sync::Mutex<auth::Secret>>,
}

/// Writes the account's sign-in to the keyring (through [`set_secret_saver`]).
fn persist(server_id: &ServerId, secret: &auth::Secret) {
    if let Some(save) = SAVER.get() {
        save(server_id, &secret.to_json());
    }
}

fn net(e: reqwest::Error) -> SourceError {
    SourceError::Network(e.to_string())
}

fn uri(id: &str) -> String {
    format!("spotify:track:{id}")
}

/// Spotify's error object (`{"error": {"status": 403, "message": "…"}}`).
fn error_message(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            v.pointer("/error/message")
                .and_then(Value::as_str)
                .or_else(|| v.get("error").and_then(Value::as_str))
                .map(str::to_string)
        })
        .unwrap_or_else(|| body.chars().take(200).collect())
}

impl Spotify {
    /// Signs in with a stored [`auth::Secret`] (JSON).
    pub async fn connect(server_id: ServerId, secret: &str) -> Result<Self> {
        let secret = auth::Secret::from_json(secret)
            .ok_or_else(|| SourceError::Auth("no Spotify sign-in stored; sign in again".into()))?;
        let http = reqwest::Client::builder().timeout(Duration::from_secs(30)).build().map_err(net)?;
        let tokens = auth::refresh(&http, &secret.client_id, &secret.refresh_token).await?;
        let refresh = tokens.refresh_token.clone().unwrap_or(secret.refresh_token.clone());
        let stored = Arc::new(std::sync::Mutex::new(secret.clone()));
        let player = secret.player.clone().map(|login| {
            let (stored, id) = (stored.clone(), server_id.clone());
            let on_new_login: Box<dyn Fn(auth::PlayerLogin) + Send + Sync> = Box::new(move |login| {
                let mut s = stored.lock().unwrap_or_else(|e| e.into_inner());
                s.player = Some(login);
                persist(&id, &s);
            });
            player::Player::new(login, Some(on_new_login))
        });
        let s = Self {
            server_id,
            client_id: secret.client_id.clone(),
            http,
            token: Mutex::new(TokenState {
                access: tokens.access_token,
                refresh: refresh.clone(),
                expires: Instant::now() + Duration::from_secs(tokens.expires_in),
            }),
            me: std::sync::Mutex::default(),
            liked: Mutex::default(),
            liked_ids: std::sync::Mutex::default(),
            albums: Mutex::default(),
            artists: Mutex::default(),
            yt: tokio::sync::OnceCell::new(),
            matches: std::sync::Mutex::default(),
            quality: std::sync::atomic::AtomicU8::new(0),
            player,
            youtube_only: std::sync::Mutex::default(),
            stored,
        };
        if refresh != secret.refresh_token {
            s.save_refresh_token(&refresh);
        }
        Ok(s)
    }

    /// The account, once [`MusicSource::ping`] has run.
    pub fn me(&self) -> Me {
        self.me.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The sign-in to store now (with the latest refresh token).
    pub async fn secret(&self) -> String {
        let refresh_token = self.token.lock().await.refresh.clone();
        let mut s = self.stored.lock().unwrap_or_else(|e| e.into_inner()).clone();
        s.refresh_token = refresh_token;
        s.to_json()
    }

    /// Songs play from Spotify itself (a player sign-in is stored).
    pub fn plays_from_spotify(&self) -> bool {
        self.player.is_some()
    }

    fn me_id(&self) -> String {
        self.me().id
    }

    fn save_refresh_token(&self, refresh: &str) {
        let mut s = self.stored.lock().unwrap_or_else(|e| e.into_inner());
        s.refresh_token = refresh.to_string();
        persist(&self.server_id, &s);
    }

    async fn access_token(&self, force: bool) -> Result<String> {
        let mut t = self.token.lock().await;
        if !force && t.expires > Instant::now() + Duration::from_secs(60) {
            return Ok(t.access.clone());
        }
        let tokens = auth::refresh(&self.http, &self.client_id, &t.refresh).await?;
        t.access = tokens.access_token;
        t.expires = Instant::now() + Duration::from_secs(tokens.expires_in);
        if let Some(new) = tokens.refresh_token.filter(|r| *r != t.refresh) {
            t.refresh = new.clone();
            self.save_refresh_token(&new);
        }
        Ok(t.access.clone())
    }

    /// One Web API call; `path` is relative to the API root or a full `next` URL. Retries once with
    /// a fresh token and waits out short rate limits. `Value::Null` for empty answers.
    async fn call(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
    ) -> Result<Value> {
        let url = if path.starts_with("http") { path.to_string() } else { format!("{API}{path}") };
        let mut refreshed = false;
        let mut waited = 0;
        loop {
            let token = self.access_token(false).await?;
            let mut req = self.http.request(method.clone(), &url).bearer_auth(token).query(query);
            req = match body {
                Some(b) => req.json(b),
                None if method != Method::GET => req.header(reqwest::header::CONTENT_LENGTH, "0"),
                None => req,
            };
            let resp = req.send().await.map_err(net)?;
            let status = resp.status();
            if status.as_u16() == 429 {
                let after = resp
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(1);
                if after <= 10 && waited < 3 {
                    waited += 1;
                    tokio::time::sleep(Duration::from_secs(after.max(1))).await;
                    continue;
                }
                return Err(SourceError::RateLimited);
            }
            let text = resp.text().await.map_err(net)?;
            match status.as_u16() {
                200..=299 if text.trim().is_empty() => return Ok(Value::Null),
                200..=299 => {
                    return serde_json::from_str(&text)
                        .map_err(|e| SourceError::Parse(format!("{path}: {e}")));
                }
                401 if !refreshed => {
                    refreshed = true;
                    self.access_token(true).await?;
                }
                401 => return Err(SourceError::Auth(error_message(&text))),
                403 => {
                    let message = error_message(&text);
                    return Err(SourceError::Server {
                        code: 403,
                        message: format!(
                            "{message} (Spotify only lets accounts added under User Management of your \
                             developer app use it)"
                        ),
                    });
                }
                404 => return Err(SourceError::NotFound),
                code => return Err(SourceError::Server { code: code as i32, message: error_message(&text) }),
            }
        }
    }

    async fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Value> {
        self.call(Method::GET, path, query, None).await
    }

    /// Every item of a paged list (`{"items": [], "total": n, "next": url}`), up to `max`. `key`
    /// picks the page out of a wrapper (`/search` answers `{"tracks": {…}}`).
    async fn all_pages(
        &self,
        path: &str,
        query: &[(&str, String)],
        page: usize,
        max: usize,
    ) -> Result<Vec<Value>> {
        let with = |offset: usize| {
            let mut q = query.to_vec();
            q.push(("limit", page.to_string()));
            q.push(("offset", offset.to_string()));
            q
        };
        let first = self.get(path, &with(0)).await?;
        let mut items: Vec<Value> = first.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
        let total = first.get("total").and_then(Value::as_u64).map(|t| (t as usize).min(max));
        match total {
            Some(total) if total > items.len() => {
                let offsets: Vec<usize> = (items.len()..total).step_by(page).collect();
                let pages: Vec<Result<Value>> = futures::stream::iter(offsets)
                    .map(|o| {
                        let q = with(o);
                        async move { self.get(path, &q).await }
                    })
                    .buffered(PARALLEL)
                    .collect()
                    .await;
                for p in pages {
                    items.extend(p?.get("items").and_then(Value::as_array).cloned().unwrap_or_default());
                }
            }
            Some(_) => {}
            None => {
                // No total: follow `next`.
                let mut next = first.get("next").and_then(Value::as_str).map(str::to_string);
                while let Some(url) = next.take().filter(|_| items.len() < max) {
                    let p = self.get(&url, &[]).await?;
                    items.extend(p.get("items").and_then(Value::as_array).cloned().unwrap_or_default());
                    next = p.get("next").and_then(Value::as_str).map(str::to_string);
                }
            }
        }
        items.truncate(max);
        Ok(items)
    }

    fn tracks_of(&self, items: &[Value]) -> Vec<Song> {
        items
            .iter()
            .filter_map(|e| convert::entry_item(e).and_then(|t| convert::track(t, &self.server_id, None)))
            .collect()
    }

    /// Marks the songs that are in Liked Songs (if those were read).
    fn mark_liked(&self, songs: &mut [Song]) {
        if let Some(ids) = self.liked_ids.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            for s in songs {
                s.favorite = ids.contains(&s.id);
            }
        }
    }

    async fn liked_list(&self) -> Result<Arc<clientside::SongList>> {
        let mut cache = self.liked.lock().await;
        if let Some(list) = Cached::fresh(&cache) {
            return Ok(list);
        }
        let items = self.all_pages("/me/tracks", &[], 50, 20_000).await?;
        let mut songs = self.tracks_of(&items);
        for s in &mut songs {
            s.favorite = true;
        }
        *self.liked_ids.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(songs.iter().map(|s| s.id.clone()).collect());
        let list = Arc::new(clientside::SongList::new(songs));
        *cache = Some(Cached { at: Instant::now(), items: list.clone() });
        Ok(list)
    }

    async fn saved_albums(&self) -> Result<Vec<Album>> {
        let mut cache = self.albums.lock().await;
        if let Some(items) = Cached::fresh(&cache) {
            return Ok(items);
        }
        let items = self.all_pages("/me/albums", &[], 50, 5_000).await?;
        let albums: Vec<Album> = items
            .iter()
            .filter_map(|e| e.get("album").and_then(|a| convert::album(a, &self.server_id)))
            .map(|mut a| {
                a.favorite = true;
                a.songs.clear();
                a
            })
            .collect();
        *cache = Some(Cached { at: Instant::now(), items: albums.clone() });
        Ok(albums)
    }

    async fn followed_artists(&self) -> Result<Vec<Artist>> {
        let mut cache = self.artists.lock().await;
        if let Some(items) = Cached::fresh(&cache) {
            return Ok(items);
        }
        // Cursor paging (`after`), wrapped in `{"artists": {…}}`.
        let mut artists = Vec::new();
        let mut next: Option<String> = None;
        loop {
            let page = match &next {
                Some(url) => self.get(url, &[]).await?,
                None => {
                    self.get("/me/following", &[("type", "artist".into()), ("limit", "50".into())]).await?
                }
            };
            let page = page.get("artists").cloned().unwrap_or(Value::Null);
            for a in page.get("items").and_then(Value::as_array).into_iter().flatten() {
                if let Some(mut artist) = convert::artist(a, &self.server_id) {
                    artist.favorite = true;
                    artists.push(artist);
                }
            }
            next = page.get("next").and_then(Value::as_str).map(str::to_string);
            if next.is_none() || artists.len() >= 5_000 {
                break;
            }
        }
        *cache = Some(Cached { at: Instant::now(), items: artists.clone() });
        Ok(artists)
    }

    async fn forget_library(&self) {
        *self.liked.lock().await = None;
        *self.albums.lock().await = None;
        *self.artists.lock().await = None;
    }

    /// The YouTube Music side, signed out (search and streams only).
    async fn yt(&self) -> Result<&Arc<YouTubeMusic>> {
        self.yt
            .get_or_try_init(|| async {
                let yt = YouTubeMusic::connect(
                    ServerId(format!("{}-youtube", self.server_id.0)),
                    listen_ytmusic::GUEST_COOKIE,
                )
                .await?;
                if let Err(e) = yt.check_ytdlp().await {
                    tracing::warn!("{e}");
                }
                yt.set_stream_quality(StreamQuality::from_u8(
                    self.quality.load(std::sync::atomic::Ordering::Relaxed),
                ));
                Ok::<_, SourceError>(Arc::new(yt))
            })
            .await
    }

    /// The YouTube Music copy of a Spotify track.
    pub async fn youtube_match(&self, song: &Song) -> Result<Song> {
        if let Some(m) = self.matches.lock().unwrap_or_else(|e| e.into_inner()).get(&song.id) {
            return Ok(m.clone());
        }
        let yt = self.yt().await?;
        let mut found = None;
        let title_only = song.title.split(" - ").next().unwrap_or(&song.title).to_string();
        for q in [matching::query(song), title_only] {
            let candidates = yt.search_songs(&q).await?;
            if let Some(best) = matching::best(song, &candidates) {
                found = Some(best.clone());
                break;
            }
        }
        let m = found.ok_or_else(|| {
            SourceError::Other(format!(
                "“{}” by {} was not found on YouTube Music",
                song.title,
                song.artist_line()
            ))
        })?;
        tracing::info!("spotify: {} → youtube {} ({})", song.id, m.id, m.title);
        self.matches.lock().unwrap_or_else(|e| e.into_inner()).insert(song.id.clone(), m.clone());
        Ok(m)
    }

    async fn playlist_items(&self, id: &str, query: &[(&str, String)]) -> Result<Value> {
        // `/items` is the current name of the endpoint; `/tracks` the old one.
        match self.get(&format!("/playlists/{id}/items"), query).await {
            Err(SourceError::NotFound) => self.get(&format!("/playlists/{id}/tracks"), query).await,
            other => other,
        }
    }

    async fn playlist_write(&self, method: Method, id: &str, body: &Value) -> Result<Value> {
        match self.call(method.clone(), &format!("/playlists/{id}/items"), &[], Some(body)).await {
            Err(SourceError::NotFound) => {
                self.call(method, &format!("/playlists/{id}/tracks"), &[], Some(body)).await
            }
            other => other,
        }
    }

    async fn all_playlist_songs(&self, id: &str) -> Result<Vec<Song>> {
        let mut songs = Vec::new();
        let mut cursor = None;
        loop {
            let (page, next) = self.playlist_page(id, cursor.as_deref()).await?;
            songs.extend(page);
            match next {
                Some(n) => cursor = Some(n),
                None => return Ok(songs),
            }
        }
    }

    /// Saves (`PUT`) or removes (`DELETE`) library items: `/me/tracks`, `/me/albums`, ...
    async fn library_edit(&self, path: &str, kind: &str, ids: &[String], on: bool) -> Result<()> {
        let method = if on { Method::PUT } else { Method::DELETE };
        for chunk in ids.chunks(50) {
            let mut q = vec![("ids", chunk.join(","))];
            if path == "/me/following" {
                q.push(("type", kind.to_string()));
            }
            match self.call(method.clone(), path, &q, None).await {
                // Newer apps save everything through `/me/library` with URIs.
                Err(SourceError::NotFound | SourceError::Server { code: 410, .. }) => {
                    let uris =
                        chunk.iter().map(|id| format!("spotify:{kind}:{id}")).collect::<Vec<_>>().join(",");
                    self.call(method.clone(), "/me/library", &[("uris", uris)], None).await?;
                }
                other => {
                    other?;
                }
            }
        }
        Ok(())
    }

    async fn search_kind(&self, text: &str, kind: &str, limit: usize) -> Result<Value> {
        let q = |limit: usize| {
            vec![("q", text.to_string()), ("type", kind.to_string()), ("limit", limit.to_string())]
        };
        match self.get("/search", &q(limit.clamp(1, 50))).await {
            // Development-mode apps get a lower search limit.
            Err(SourceError::Server { code: 400, .. }) if limit > 10 => self.get("/search", &q(10)).await,
            other => other,
        }
    }
}

#[async_trait]
impl MusicSource for Spotify {
    fn kind(&self) -> SourceKind {
        SourceKind::Spotify
    }

    fn capabilities(&self) -> Capabilities {
        capabilities()
    }

    async fn ping(&self) -> Result<()> {
        let me = self.get("/me", &[]).await?;
        let id = convert::str_of(&me, "id").ok_or_else(|| SourceError::Parse("no user id".into()))?;
        let display_name = convert::str_of(&me, "display_name").unwrap_or_else(|| id.clone());
        *self.me.lock().unwrap_or_else(|e| e.into_inner()) = Me { id, display_name };
        Ok(())
    }

    fn album_sorts(&self) -> &'static [listen_source::SortOption<listen_source::AlbumSort>] {
        listen_source::sorting::LIBRARY_ALBUMS
    }

    fn song_sorts(&self) -> &'static [listen_source::SortOption<listen_source::SongSort>] {
        listen_source::sorting::LIBRARY_SONGS
    }

    fn artist_sorts(&self) -> &'static [listen_source::SortOption<listen_source::ArtistSort>] {
        listen_source::sorting::LIBRARY_ARTISTS
    }

    fn album_filters(&self) -> listen_source::QueryFilters {
        listen_source::QueryFilters::YEAR_RANGE
    }

    fn song_filters(&self) -> listen_source::QueryFilters {
        listen_source::QueryFilters::empty()
    }

    fn artist_filters(&self) -> listen_source::QueryFilters {
        listen_source::QueryFilters::empty()
    }

    async fn refresh(&self) {
        self.forget_library().await;
    }

    async fn albums(&self, q: &AlbumQuery) -> Result<Page<Album>> {
        let items = clientside::albums(self.saved_albums().await?, q);
        Ok(clientside::page(&items, q.start, q.limit))
    }

    async fn album(&self, id: &str) -> Result<Album> {
        let v = self.get(&format!("/albums/{id}"), &[]).await?;
        let mut album = convert::album(&v, &self.server_id).ok_or(SourceError::NotFound)?;
        // Albums with more than 50 tracks continue on `/albums/{id}/tracks`.
        if v.pointer("/tracks/next").and_then(Value::as_str).is_some() {
            let items = self.all_pages(&format!("/albums/{id}/tracks"), &[], 50, 2_000).await?;
            album.songs = items.iter().filter_map(|t| convert::track(t, &self.server_id, Some(&v))).collect();
            album.duration = album.songs.iter().map(|s| s.duration).sum();
        }
        self.mark_liked(&mut album.songs);
        if let Some(saved) = self.albums.lock().await.as_ref() {
            album.favorite = saved.items.iter().any(|a| a.id == album.id);
        }
        Ok(album)
    }

    async fn artists(&self, q: &ListQuery) -> Result<Page<Artist>> {
        let query =
            ArtistQuery { search: q.search.clone(), start: q.start, limit: q.limit, ..Default::default() };
        self.artists_query(&query).await
    }

    async fn artists_query(&self, q: &ArtistQuery) -> Result<Page<Artist>> {
        let items = clientside::artists(self.followed_artists().await?, q);
        Ok(clientside::page(&items, q.start, q.limit))
    }

    async fn artist(&self, id: &str) -> Result<Artist> {
        let v = self.get(&format!("/artists/{id}"), &[]).await?;
        let mut artist = convert::artist(&v, &self.server_id).ok_or(SourceError::NotFound)?;
        if let Some(followed) = self.artists.lock().await.as_ref() {
            artist.favorite = followed.items.iter().any(|a| a.id == artist.id);
        }
        Ok(artist)
    }

    async fn artist_albums(&self, id: &str) -> Result<Vec<Album>> {
        let q = [("include_groups", "album,single,compilation".to_string())];
        let items = self.all_pages(&format!("/artists/{id}/albums"), &q, 50, 500).await?;
        Ok(items.iter().filter_map(|a| convert::album(a, &self.server_id)).collect())
    }

    async fn top_songs(&self, artist: &Artist, count: usize) -> Result<Vec<Song>> {
        let top =
            self.get(&format!("/artists/{}/top-tracks", artist.id), &[("market", "from_token".into())]).await;
        let mut songs: Vec<Song> = match top {
            Ok(v) => v
                .get("tracks")
                .and_then(Value::as_array)
                .map(|t| t.iter().filter_map(|t| convert::track(t, &self.server_id, None)).collect())
                .unwrap_or_default(),
            // Not offered to every app any more: search the artist's tracks instead.
            Err(e) => {
                tracing::debug!("top tracks: {e}; searching instead");
                let v =
                    self.search_kind(&format!("artist:\"{}\"", artist.name), "track", count.max(10)).await?;
                v.pointer("/tracks/items")
                    .and_then(Value::as_array)
                    .map(|t| {
                        t.iter().filter_map(|t| convert::track(t, &self.server_id, None)).collect::<Vec<_>>()
                    })
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|s| listen_source::is_by(s, artist))
                    .collect()
            }
        };
        songs.truncate(count);
        self.mark_liked(&mut songs);
        Ok(songs)
    }

    async fn songs(&self, q: &SongQuery) -> Result<Page<Song>> {
        let items = self.liked_list().await?.query(q);
        Ok(clientside::page(&items, q.start, q.limit))
    }

    async fn random_songs(&self, count: usize, _genre: Option<&str>) -> Result<Vec<Song>> {
        use rand::seq::SliceRandom;
        let liked = self.liked_list().await?;
        let mut songs = liked.songs().to_vec();
        songs.shuffle(&mut rand::rng());
        songs.truncate(count);
        Ok(songs)
    }

    async fn search(&self, text: &str, limit: usize) -> Result<SearchResults> {
        let v = self.search_kind(text, "track,album,artist,playlist", limit).await?;
        let list = |key: &str| {
            v.pointer(&format!("/{key}/items")).and_then(Value::as_array).cloned().unwrap_or_default()
        };
        let me = self.me_id();
        let mut songs: Vec<Song> =
            list("tracks").iter().filter_map(|t| convert::track(t, &self.server_id, None)).collect();
        self.mark_liked(&mut songs);
        Ok(SearchResults {
            songs,
            albums: list("albums").iter().filter_map(|a| convert::album(a, &self.server_id)).collect(),
            artists: list("artists").iter().filter_map(|a| convert::artist(a, &self.server_id)).collect(),
            playlists: list("playlists")
                .iter()
                .filter_map(|p| convert::playlist(p, &self.server_id, &me))
                .collect(),
        })
    }

    async fn home(&self) -> Result<Vec<HomeShelf>> {
        let shelf =
            |title: &str| HomeShelf { title: title.into(), albums: vec![], playlists: vec![], songs: vec![] };
        let recent_q = [("limit", "50".to_string())];
        let top_q = [("limit", "30".to_string()), ("time_range", "short_term".to_string())];
        let (recent, top, playlists, albums) = tokio::join!(
            self.get("/me/player/recently-played", &recent_q),
            self.get("/me/top/tracks", &top_q),
            self.playlists(),
            self.saved_albums(),
        );
        let mut shelves = Vec::new();
        if let Ok(v) = recent {
            let mut seen = HashSet::new();
            let items = v.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
            let mut songs: Vec<Song> =
                self.tracks_of(&items).into_iter().filter(|s| seen.insert(s.id.clone())).collect();
            self.mark_liked(&mut songs);
            if !songs.is_empty() {
                shelves.push(HomeShelf { songs, ..shelf("Recently played") });
            }
        }
        if let Ok(v) = top {
            let mut songs: Vec<Song> = v
                .get("items")
                .and_then(Value::as_array)
                .map(|t| t.iter().filter_map(|t| convert::track(t, &self.server_id, None)).collect())
                .unwrap_or_default();
            self.mark_liked(&mut songs);
            if !songs.is_empty() {
                shelves.push(HomeShelf { songs, ..shelf("Your top songs") });
            }
        }
        if let Ok(p) = playlists
            && !p.is_empty()
        {
            shelves
                .push(HomeShelf { playlists: p.into_iter().take(20).collect(), ..shelf("Your playlists") });
        }
        if let Ok(a) = albums
            && !a.is_empty()
        {
            shelves.push(HomeShelf { albums: a.into_iter().take(20).collect(), ..shelf("Saved albums") });
        }
        Ok(shelves)
    }

    async fn playlists(&self) -> Result<Vec<Playlist>> {
        let me = self.me_id();
        let items = self.all_pages("/me/playlists", &[], 50, 5_000).await?;
        let mut out = vec![Playlist {
            id: LIKED_PLAYLIST.into(),
            server_id: self.server_id.clone(),
            source: SourceKind::Spotify,
            name: "Liked Songs".into(),
            description: None,
            owner: Some(self.me().display_name).filter(|n| !n.is_empty()),
            image: None,
            song_count: None,
            duration: Duration::ZERO,
            public: false,
            editable: false,
        }];
        out.extend(items.iter().filter_map(|p| convert::playlist(p, &self.server_id, &me)));
        Ok(out)
    }

    async fn playlist_songs(&self, id: &str) -> Result<Vec<Song>> {
        if id == LIKED_PLAYLIST {
            return Ok(self.liked_list().await?.songs().to_vec());
        }
        self.all_playlist_songs(id).await
    }

    async fn playlist_page(&self, id: &str, cursor: Option<&str>) -> Result<(Vec<Song>, Option<String>)> {
        if id == LIKED_PLAYLIST {
            return match cursor {
                None => Ok((self.playlist_songs(id).await?, None)),
                Some(_) => Ok((vec![], None)),
            };
        }
        let offset: usize = cursor.and_then(|c| c.parse().ok()).unwrap_or(0);
        let page = self
            .playlist_items(id, &[("limit", PLAYLIST_PAGE.to_string()), ("offset", offset.to_string())])
            .await?;
        let items = page.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
        let total = page.get("total").and_then(Value::as_u64).map(|t| t as usize);
        let mut songs = self.tracks_of(&items);
        self.mark_liked(&mut songs);
        let end = offset + items.len();
        let next = (!items.is_empty() && total.is_none_or(|t| end < t)).then(|| end.to_string());
        Ok((songs, next))
    }

    async fn create_playlist(&self, name: &str, songs: &[String]) -> Result<Playlist> {
        let body = json!({"name": name, "public": false});
        let v = match self.call(Method::POST, "/me/playlists", &[], Some(&body)).await {
            Err(SourceError::NotFound) => {
                let me = self.me_id();
                self.call(Method::POST, &format!("/users/{me}/playlists"), &[], Some(&body)).await?
            }
            other => other?,
        };
        let playlist = convert::playlist(&v, &self.server_id, &self.me_id())
            .ok_or_else(|| SourceError::Parse("created playlist without id".into()))?;
        if !songs.is_empty() {
            self.add_to_playlist(&playlist.id, songs).await?;
        }
        Ok(Playlist { editable: true, ..playlist })
    }

    async fn add_to_playlist(&self, id: &str, songs: &[String]) -> Result<()> {
        if id == LIKED_PLAYLIST {
            return self.set_favorite(songs, true).await;
        }
        for chunk in songs.chunks(100) {
            let uris: Vec<String> = chunk.iter().map(|s| uri(s)).collect();
            self.playlist_write(Method::POST, id, &json!({ "uris": uris })).await?;
        }
        Ok(())
    }

    /// Spotify removes by track, so every copy of a removed song leaves the playlist.
    async fn remove_from_playlist(&self, id: &str, indices: &[usize]) -> Result<()> {
        let songs = self.playlist_songs(id).await?;
        if id == LIKED_PLAYLIST {
            let ids: Vec<String> =
                indices.iter().filter_map(|i| songs.get(*i)).map(|s| s.id.clone()).collect();
            return self.set_favorite(&ids, false).await;
        }
        let uris: Vec<Value> =
            indices.iter().filter_map(|i| songs.get(*i)).map(|s| json!({ "uri": uri(&s.id) })).collect();
        for chunk in uris.chunks(100) {
            self.playlist_write(Method::DELETE, id, &json!({ "tracks": chunk, "items": chunk })).await?;
        }
        Ok(())
    }

    async fn move_in_playlist(&self, id: &str, from: usize, to: usize) -> Result<()> {
        let insert_before = if to > from { to + 1 } else { to };
        self.playlist_write(Method::PUT, id, &json!({ "range_start": from, "insert_before": insert_before }))
            .await?;
        Ok(())
    }

    async fn delete_playlist(&self, id: &str) -> Result<()> {
        // Spotify has no deleting: the owner unfollows it.
        self.call(Method::DELETE, &format!("/playlists/{id}/followers"), &[], None).await?;
        Ok(())
    }

    async fn rename_playlist(&self, id: &str, name: &str) -> Result<()> {
        self.update_playlist(id, &PlaylistUpdate { name: Some(name.to_string()), ..Default::default() }).await
    }

    async fn update_playlist(&self, id: &str, update: &PlaylistUpdate) -> Result<()> {
        let mut body = serde_json::Map::new();
        if let Some(name) = &update.name {
            body.insert("name".into(), json!(name));
        }
        if let Some(comment) = &update.comment {
            body.insert("description".into(), json!(comment));
        }
        if let Some(public) = update.public {
            body.insert("public".into(), json!(public));
        }
        if body.is_empty() {
            return Ok(());
        }
        self.call(Method::PUT, &format!("/playlists/{id}"), &[], Some(&Value::Object(body))).await?;
        Ok(())
    }

    async fn set_favorite(&self, song_ids: &[String], favorite: bool) -> Result<()> {
        self.library_edit("/me/tracks", "track", song_ids, favorite).await?;
        *self.liked.lock().await = None;
        if let Some(ids) = self.liked_ids.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            for id in song_ids {
                if favorite {
                    ids.insert(id.clone());
                } else {
                    ids.remove(id);
                }
            }
        }
        Ok(())
    }

    async fn set_favorite_items(&self, kind: ItemKind, ids: &[String], favorite: bool) -> Result<()> {
        match kind {
            ItemKind::Song => self.set_favorite(ids, favorite).await,
            ItemKind::Album => {
                self.library_edit("/me/albums", "album", ids, favorite).await?;
                *self.albums.lock().await = None;
                Ok(())
            }
            ItemKind::Artist => {
                self.library_edit("/me/following", "artist", ids, favorite).await?;
                *self.artists.lock().await = None;
                Ok(())
            }
        }
    }

    async fn favorite_albums(&self) -> Result<Vec<Album>> {
        self.saved_albums().await
    }

    async fn favorite_artists(&self) -> Result<Vec<Artist>> {
        self.followed_artists().await
    }

    fn set_stream_quality(&self, quality: StreamQuality) {
        self.quality.store(quality.to_u8(), std::sync::atomic::Ordering::Relaxed);
        if let Some(yt) = self.yt.get() {
            yt.set_stream_quality(quality);
        }
    }

    async fn stream(&self, song: &Song) -> Result<StreamSource> {
        let youtube_only = self.youtube_only.lock().unwrap_or_else(|e| e.into_inner()).contains(&song.id);
        if let Some(player) = self.player.as_ref().filter(|_| !youtube_only) {
            let quality = StreamQuality::from_u8(self.quality.load(std::sync::atomic::Ordering::Relaxed));
            match player.url(&song.id, quality).await {
                Ok(url) => return Ok(StreamSource::direct(url)),
                Err(e) => {
                    tracing::warn!(
                        "spotify: “{}” from Spotify failed ({e}); using YouTube Music",
                        song.title
                    );
                    self.youtube_only.lock().unwrap_or_else(|e| e.into_inner()).insert(song.id.clone());
                }
            }
        }
        let m = self.youtube_match(song).await?;
        self.yt().await?.stream(&m).await
    }

    fn forget_stream(&self, song: &Song) {
        // Spotify's file failed in mpv: the retry plays the YouTube Music copy.
        if let Some(player) = &self.player
            && player.loudness(&song.id).is_some()
        {
            player.forget(&song.id);
            self.youtube_only.lock().unwrap_or_else(|e| e.into_inner()).insert(song.id.clone());
            return;
        }
        let m = self.matches.lock().unwrap_or_else(|e| e.into_inner()).get(&song.id).cloned();
        if let (Some(m), Some(yt)) = (m, self.yt.get()) {
            yt.forget_stream(&m);
        }
    }

    fn alternate_url(&self, url: &str) -> Option<String> {
        listen_ytmusic::ytdlp::fallback_url(url)
    }

    fn share_url(&self, song: &Song) -> Option<String> {
        Some(format!("https://open.spotify.com/track/{}", song.id))
    }

    fn image_url(&self, image: &ImageRef, _size: u32) -> Option<String> {
        match image {
            ImageRef::Url(url) => Some(url.clone()),
            ImageRef::Id(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_spotify_errors() {
        assert_eq!(error_message(r#"{"error":{"status":403,"message":"Forbidden"}}"#), "Forbidden");
        assert_eq!(error_message(r#"{"error":"invalid_client"}"#), "invalid_client");
        assert_eq!(error_message("plain"), "plain");
    }

    #[test]
    fn edits_and_favourites_are_offered() {
        let c = capabilities();
        assert!(
            c.contains(Capabilities::PLAYLIST_EDIT | Capabilities::FAVORITE_ITEMS | Capabilities::HOME_FEED)
        );
        assert!(!c.contains(Capabilities::RATINGS));
    }
}
