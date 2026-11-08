//! Playback from Spotify itself (Premium), the way librespot does it: Listen signs in to Spotify's
//! audio servers with Spotify's player client, downloads a song's Ogg Vorbis file, decrypts it and
//! hands mpv the plain Ogg stream from a small HTTP server on 127.0.0.1. mpv seeks with range
//! requests, so crossfades, DJ mixing, levelling and the visualizer work as for any other song.

use crate::auth::{self, PlayerLogin};
use base64::Engine;
use librespot_audio::{AudioDecrypt, AudioFile};
use librespot_core::authentication::Credentials;
use librespot_core::{FileId, Session, SessionConfig, SpotifyId, SpotifyUri};
use librespot_metadata::audio::{AudioFileFormat, AudioItem};
use librespot_protocol::authentication::AuthenticationType;
use listen_core::settings::StreamQuality;
use std::collections::VecDeque;
use std::io::{Read, Seek, SeekFrom};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Spotify's Ogg files start with a header of its own; the Ogg stream begins after it.
const OGG_HEADER_END: u64 = 0xa7;
/// Where the header keeps the loudness (track gain dB, track peak, album gain dB, album peak).
const NORMALISATION_OFFSET: u64 = 144;
/// Songs kept open (the playing one, the next, a few back for quick replays).
const OPEN_FILES: usize = 6;
const CHUNK: usize = 64 * 1024;

type Saver = dyn Fn(PlayerLogin) + Send + Sync;

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

fn login_of(session: &Session) -> PlayerLogin {
    PlayerLogin { username: session.username(), auth_data: b64().encode(session.auth_data()) }
}

/// Spotify sends the account type a moment after the login; free accounts get no audio.
async fn check_premium(session: &Session) -> Result<(), String> {
    for _ in 0..30 {
        match session.get_user_attribute("type").as_deref() {
            Some("premium") => return Ok(()),
            Some(other) => {
                return Err(format!(
                    "Playing from Spotify needs Spotify Premium (this account is “{other}”); songs play \
                     from YouTube Music instead."
                ));
            }
            None => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
    Ok(())
}

/// Signs in to Spotify's audio servers in the browser (a second consent, for Spotify's own player
/// client) and returns the reusable login to store.
pub async fn sign_in(open: impl FnOnce(&str)) -> Result<PlayerLogin, String> {
    let tokens = auth::authorize(&auth::Flow::player(), open).await?;
    let session = Session::new(SessionConfig::default(), None);
    session
        .connect(Credentials::with_access_token(tokens.access_token), false)
        .await
        .map_err(|e| format!("Spotify's audio servers refused the sign-in: {e}"))?;
    check_premium(&session).await?;
    let login = login_of(&session);
    session.shutdown();
    Ok(login)
}

/// Loudness Spotify stores for a song (ReplayGain style).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Loudness {
    pub track_gain_db: f32,
    pub track_peak: f32,
    pub album_gain_db: f32,
    pub album_peak: f32,
}

fn parse_loudness(b: &[u8; 16]) -> Loudness {
    let f = |i: usize| f32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
    Loudness { track_gain_db: f(0), track_peak: f(4), album_gain_db: f(8), album_peak: f(12) }
}

/// One decrypted song file, read by the HTTP server.
struct Open {
    track: String,
    file: Mutex<AudioDecrypt<AudioFile>>,
    /// Bytes of the Ogg stream (the file without Spotify's header).
    len: u64,
    loudness: Option<Loudness>,
}

pub struct Player {
    login: Mutex<PlayerLogin>,
    on_new_login: Option<Box<Saver>>,
    session: tokio::sync::Mutex<Option<Session>>,
    open: Mutex<VecDeque<Arc<Open>>>,
    port: tokio::sync::OnceCell<u16>,
}

fn formats(quality: StreamQuality) -> &'static [AudioFileFormat] {
    use AudioFileFormat::*;
    match quality {
        StreamQuality::Original | StreamQuality::High => &[OGG_VORBIS_320, OGG_VORBIS_160, OGG_VORBIS_96],
        StreamQuality::Normal => &[OGG_VORBIS_160, OGG_VORBIS_320, OGG_VORBIS_96],
        StreamQuality::Low => &[OGG_VORBIS_96, OGG_VORBIS_160, OGG_VORBIS_320],
    }
}

fn bytes_per_second(format: AudioFileFormat) -> usize {
    let kbps = match format {
        AudioFileFormat::OGG_VORBIS_96 => 12,
        AudioFileFormat::OGG_VORBIS_160 => 20,
        _ => 40,
    };
    kbps * 1024
}

impl Player {
    /// `on_new_login`: Spotify replaced the stored login (store the new one).
    pub fn new(login: PlayerLogin, on_new_login: Option<Box<Saver>>) -> Arc<Self> {
        Arc::new(Self {
            login: Mutex::new(login),
            on_new_login,
            session: tokio::sync::Mutex::new(None),
            open: Mutex::default(),
            port: tokio::sync::OnceCell::new(),
        })
    }

    pub fn login(&self) -> PlayerLogin {
        self.login.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The connected session, signing in again when Spotify dropped it.
    async fn session(&self) -> Result<Session, String> {
        let mut slot = self.session.lock().await;
        if let Some(s) = slot.as_ref().filter(|s| !s.is_invalid()) {
            return Ok(s.clone());
        }
        let login = self.login();
        let auth_data = b64().decode(&login.auth_data).map_err(|e| format!("stored Spotify login: {e}"))?;
        let credentials = Credentials {
            username: Some(login.username.clone()),
            auth_type: AuthenticationType::AUTHENTICATION_STORED_SPOTIFY_CREDENTIALS,
            auth_data,
        };
        let session = Session::new(SessionConfig::default(), None);
        session.connect(credentials, false).await.map_err(|e| {
            format!("Spotify's audio servers refused the stored login ({e}); sign in for playback again")
        })?;
        check_premium(&session).await?;
        let fresh = login_of(&session);
        if fresh != login {
            *self.login.lock().unwrap_or_else(|e| e.into_inner()) = fresh.clone();
            if let Some(save) = &self.on_new_login {
                save(fresh);
            }
        }
        *slot = Some(session.clone());
        Ok(session)
    }

    fn opened(&self, track: &str) -> Option<Arc<Open>> {
        self.open.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|o| o.track == track).cloned()
    }

    /// Drops a song's open file (a failed stream is fetched again from scratch).
    pub fn forget(&self, track: &str) {
        self.open.lock().unwrap_or_else(|e| e.into_inner()).retain(|o| o.track != track);
    }

    /// The loudness of an opened song.
    pub fn loudness(&self, track: &str) -> Option<Loudness> {
        self.opened(track).and_then(|o| o.loudness)
    }

    /// Opens the song's audio and returns the local URL mpv plays it from.
    pub async fn url(self: &Arc<Self>, track: &str, quality: StreamQuality) -> Result<String, String> {
        if self.opened(track).is_none() {
            let open = self.open_track(track, quality).await?;
            let mut list = self.open.lock().unwrap_or_else(|e| e.into_inner());
            list.retain(|o| o.track != track);
            list.push_back(Arc::new(open));
            while list.len() > OPEN_FILES {
                list.pop_front();
            }
        }
        let port = self.server().await?;
        Ok(format!("http://127.0.0.1:{port}/spotify/{track}.ogg"))
    }

    async fn open_track(&self, track: &str, quality: StreamQuality) -> Result<Open, String> {
        let session = self.session().await?;
        let uri = SpotifyUri::from_uri(&format!("spotify:track:{track}")).map_err(|e| e.to_string())?;
        let mut item = AudioItem::get_file(&session, uri).await.map_err(|e| format!("track info: {e}"))?;
        if let Err(e) = &item.availability {
            return Err(format!("not playable on Spotify here ({e:?})"));
        }
        // A track without files plays from one of its relinked alternatives.
        if item.files.is_empty()
            && let Some(alternatives) = item.alternatives.clone()
        {
            for alt in alternatives.0 {
                if let Ok(alt) = AudioItem::get_file(&session, alt).await
                    && alt.availability.is_ok()
                    && !alt.files.is_empty()
                {
                    item = alt;
                    break;
                }
            }
        }
        let (format, file_id): (AudioFileFormat, FileId) = formats(quality)
            .iter()
            .find_map(|f| item.files.get(f).map(|id| (*f, *id)))
            .ok_or("no Ogg Vorbis file for this song")?;
        let spotify_id = SpotifyId::try_from(&item.track_id).map_err(|e| e.to_string())?;
        let file = AudioFile::open(&session, file_id, bytes_per_second(format))
            .await
            .map_err(|e| format!("audio file: {e}"))?;
        let size = file.get_stream_loader_controller().map_err(|e| e.to_string())?.len() as u64;
        let key =
            session.audio_key().request(spotify_id, file_id).await.map_err(|e| format!("audio key: {e}"))?;
        let mut decrypted = AudioDecrypt::new(Some(key), file);
        // The header (and the loudness in it) is at the start, which the fetch begins with anyway.
        // Reading waits for the download, so off the async threads.
        let (decrypted, loudness) = tokio::task::spawn_blocking(move || {
            let mut buf = [0u8; 16];
            let loudness = decrypted
                .seek(SeekFrom::Start(NORMALISATION_OFFSET))
                .and_then(|_| decrypted.read_exact(&mut buf))
                .ok()
                .map(|_| parse_loudness(&buf));
            (decrypted, loudness)
        })
        .await
        .map_err(|e| e.to_string())?;
        tracing::info!("spotify: playing {track} from Spotify ({format:?}, {size} bytes)");
        Ok(Open {
            track: track.to_string(),
            file: Mutex::new(decrypted),
            len: size.saturating_sub(OGG_HEADER_END),
            loudness,
        })
    }

    /// The local HTTP server's port (started on first use).
    async fn server(self: &Arc<Self>) -> Result<u16, String> {
        let me = Arc::downgrade(self);
        self.port
            .get_or_try_init(|| async move {
                let listener =
                    tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.map_err(|e| e.to_string())?;
                let port = listener.local_addr().map_err(|e| e.to_string())?.port();
                tokio::spawn(async move {
                    while let Ok((stream, _)) = listener.accept().await {
                        let Some(player) = me.upgrade() else { break };
                        tokio::spawn(async move {
                            if let Err(e) = player.respond(stream).await {
                                tracing::debug!("spotify stream: {e}");
                            }
                        });
                    }
                });
                Ok::<_, String>(port)
            })
            .await
            .copied()
    }

    async fn respond(&self, mut stream: tokio::net::TcpStream) -> std::io::Result<()> {
        let head = read_head(&mut stream).await?;
        let Some(req) = parse_request(&head) else {
            return stream.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n").await;
        };
        let Some(open) = self.opened(&req.track) else {
            return stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n").await;
        };
        let Some((start, end)) = byte_range(req.range, open.len) else {
            let msg =
                format!("HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{}\r\n\r\n", open.len);
            return stream.write_all(msg.as_bytes()).await;
        };
        let status = if req.range.is_some() { "206 Partial Content" } else { "200 OK" };
        let mut header = format!(
            "HTTP/1.1 {status}\r\nContent-Type: audio/ogg\r\nAccept-Ranges: bytes\r\nContent-Length: {}\r\nConnection: close\r\n",
            end + 1 - start
        );
        if req.range.is_some() {
            header.push_str(&format!("Content-Range: bytes {start}-{end}/{}\r\n", open.len));
        }
        header.push_str("\r\n");
        stream.write_all(header.as_bytes()).await?;
        if req.head_only {
            return Ok(());
        }
        let mut pos = start;
        while pos <= end {
            let want = ((end + 1 - pos) as usize).min(CHUNK);
            let file = open.clone();
            // The file blocks until the downloader has the bytes.
            let chunk = tokio::task::spawn_blocking(move || -> std::io::Result<Vec<u8>> {
                let mut f = file.file.lock().unwrap_or_else(|e| e.into_inner());
                f.seek(SeekFrom::Start(pos + OGG_HEADER_END))?;
                let mut buf = vec![0u8; want];
                let mut filled = 0;
                while filled < want {
                    match f.read(&mut buf[filled..])? {
                        0 => break,
                        n => filled += n,
                    }
                }
                buf.truncate(filled);
                Ok(buf)
            })
            .await
            .map_err(std::io::Error::other)??;
            if chunk.is_empty() {
                break;
            }
            stream.write_all(&chunk).await?;
            pos += chunk.len() as u64;
        }
        Ok(())
    }
}

async fn read_head(stream: &mut tokio::net::TcpStream) -> std::io::Result<String> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    while buf.len() < 16 * 1024 {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

#[derive(Debug, PartialEq)]
struct Request {
    track: String,
    head_only: bool,
    /// `bytes=a-b` as (a, b); `b` missing = to the end; `a` missing = the last `b` bytes.
    range: Option<(Option<u64>, Option<u64>)>,
}

fn parse_request(head: &str) -> Option<Request> {
    let mut lines = head.lines();
    let mut first = lines.next()?.split_whitespace();
    let method = first.next()?;
    let path = first.next()?;
    if method != "GET" && method != "HEAD" {
        return None;
    }
    let track = path.strip_prefix("/spotify/")?.split(['.', '?']).next()?.to_string();
    if track.is_empty() || !track.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    let range = lines
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("range"))
        .and_then(|(_, v)| v.trim().strip_prefix("bytes="))
        .and_then(|v| v.split(',').next())
        .and_then(|v| v.split_once('-'))
        .map(|(a, b)| (a.trim().parse().ok(), b.trim().parse().ok()));
    Some(Request { track, head_only: method == "HEAD", range })
}

/// The inclusive byte range to send, or `None` when it lies outside the file.
fn byte_range(range: Option<(Option<u64>, Option<u64>)>, len: u64) -> Option<(u64, u64)> {
    if len == 0 {
        return None;
    }
    let (start, end) = match range {
        None | Some((None, None)) => (0, len - 1),
        Some((Some(a), b)) => (a, b.unwrap_or(len - 1).min(len - 1)),
        Some((None, Some(suffix))) => (len.saturating_sub(suffix), len - 1),
    };
    (start <= end && start < len).then_some((start, end))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_range_requests() {
        let r = parse_request(
            "GET /spotify/4uLU6hMCjMI75M1A2tKUQC.ogg HTTP/1.1\r\nHost: x\r\nRange: bytes=100-\r\n\r\n",
        )
        .unwrap();
        assert_eq!(r.track, "4uLU6hMCjMI75M1A2tKUQC");
        assert_eq!(r.range, Some((Some(100), None)));
        assert!(!r.head_only);
        let r = parse_request("HEAD /spotify/abc.ogg HTTP/1.1\r\n\r\n").unwrap();
        assert!(r.head_only && r.range.is_none());
        assert!(parse_request("GET /spotify/../etc HTTP/1.1\r\n\r\n").is_none());
        assert!(parse_request("POST /spotify/abc.ogg HTTP/1.1\r\n\r\n").is_none());
    }

    #[test]
    fn byte_ranges_stay_inside_the_file() {
        assert_eq!(byte_range(None, 1000), Some((0, 999)));
        assert_eq!(byte_range(Some((Some(100), None)), 1000), Some((100, 999)));
        assert_eq!(byte_range(Some((Some(100), Some(5000))), 1000), Some((100, 999)));
        assert_eq!(byte_range(Some((None, Some(10))), 1000), Some((990, 999)));
        assert_eq!(byte_range(Some((Some(1000), None)), 1000), None);
        assert_eq!(byte_range(None, 0), None);
    }

    #[test]
    fn reads_spotify_loudness() {
        let mut b = [0u8; 16];
        b[0..4].copy_from_slice(&(-6.5f32).to_le_bytes());
        b[4..8].copy_from_slice(&0.98f32.to_le_bytes());
        b[8..12].copy_from_slice(&(-7.0f32).to_le_bytes());
        b[12..16].copy_from_slice(&1.0f32.to_le_bytes());
        let l = parse_loudness(&b);
        assert_eq!((l.track_gain_db, l.track_peak, l.album_gain_db, l.album_peak), (-6.5, 0.98, -7.0, 1.0));
    }
}
