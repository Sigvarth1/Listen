//! macOS Now Playing and media keys through `souvlaki` (MPNowPlayingInfoCenter and
//! MPRemoteCommandCenter), with the same interface as the Linux `mpris` module
//! ([`Message::Mpris`] in, [`MprisHandle`] out).
//!
//! The MediaPlayer framework is used from the main thread only: every call is sent to the main
//! dispatch queue, which the app's event loop (winit / AppKit) drains.

use crate::app::Message;
use dispatch2::DispatchQueue;
use iced::Subscription;
use iced::futures::{SinkExt, Stream, StreamExt, channel::mpsc};
use souvlaki::{
    MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig,
    SeekDirection,
};
use std::cell::RefCell;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub use crate::media_types::*;

/// Seconds the fast-forward and rewind buttons jump.
const SEEK_STEP: f64 = 10.0;

/// Maps what the system reports to the app's commands.
pub fn command(event: MediaControlEvent) -> Option<MprisCmd> {
    let sign = |d: SeekDirection| if d == SeekDirection::Forward { 1.0 } else { -1.0 };
    Some(match event {
        MediaControlEvent::Play => MprisCmd::Play,
        MediaControlEvent::Pause => MprisCmd::Pause,
        MediaControlEvent::Toggle => MprisCmd::PlayPause,
        MediaControlEvent::Next => MprisCmd::Next,
        MediaControlEvent::Previous => MprisCmd::Previous,
        MediaControlEvent::Stop => MprisCmd::Stop,
        MediaControlEvent::Seek(d) => MprisCmd::SeekBy(sign(d) * SEEK_STEP),
        MediaControlEvent::SeekBy(d, by) => MprisCmd::SeekBy(sign(d) * by.as_secs_f64()),
        MediaControlEvent::SetPosition(MediaPosition(p)) => MprisCmd::SetPosition(p.as_secs_f64()),
        MediaControlEvent::SetVolume(v) => MprisCmd::Volume(v),
        MediaControlEvent::Raise => MprisCmd::Raise,
        MediaControlEvent::OpenUri(_) | MediaControlEvent::Quit => return None,
    })
}

/// The controls and what they show; lives on the main thread.
struct Live {
    controls: MediaControls,
    current: MprisState,
}

thread_local! {
    static LIVE: RefCell<Option<Live>> = const { RefCell::new(None) };
}

/// Runs `f` with the controls on the main thread (later; never blocks the caller).
fn on_main(f: impl FnOnce(&mut Live) + Send + 'static) {
    DispatchQueue::main().exec_async(move || {
        LIVE.with(|live| {
            if let Some(live) = live.borrow_mut().as_mut() {
                f(live);
            }
        });
    });
}

/// Lets the app publish state; cheap to clone.
#[derive(Clone)]
pub struct MprisHandle {
    state: Arc<Mutex<MprisState>>,
}

impl std::fmt::Debug for MprisHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MprisHandle")
    }
}

impl MprisHandle {
    /// Stores the new state and shows it.
    pub async fn publish(self, new: MprisState) {
        *self.state.lock().expect("now playing state") = new.clone();
        on_main(move |live| {
            apply(&mut live.controls, &live.current, &new, false);
            live.current = new;
        });
    }

    /// Tells the system the position jumped (after a seek).
    pub async fn seeked(self, position_secs: f64) {
        self.state.lock().expect("now playing state").position_secs = position_secs;
        on_main(move |live| {
            live.current.position_secs = position_secs;
            let _ = live.controls.set_playback(playback(&live.current));
        });
    }

    /// Updates the position without telling the system (it extrapolates while playing).
    pub fn set_position(&self, position_secs: f64) {
        self.state.lock().expect("now playing state").position_secs = position_secs;
    }
}

#[derive(Debug, Clone)]
pub enum MprisMsg {
    Ready(MprisHandle),
    Command(MprisCmd),
}

fn duration(secs: f64) -> Duration {
    Duration::from_secs_f64(secs.max(0.0))
}

fn playback(state: &MprisState) -> MediaPlayback {
    let progress = Some(MediaPosition(duration(state.position_secs)));
    match state.status {
        Status::Playing => MediaPlayback::Playing { progress },
        Status::Paused => MediaPlayback::Paused { progress },
        Status::Stopped => MediaPlayback::Stopped,
    }
}

/// `file:///a b/c.jpg` → `file:///a%20b/c.jpg`: `NSURL URLWithString:` rejects unescaped
/// characters, and Listen's cache lives under "Application Support".
pub fn escape_file_url(url: &str) -> String {
    let Some(path) = url.strip_prefix("file://") else { return url.to_string() };
    let mut out = String::from("file://");
    for b in path.bytes() {
        if b.is_ascii_alphanumeric() || b"/-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn apply(controls: &mut MediaControls, old: &MprisState, new: &MprisState, force: bool) {
    if force || old.track != new.track {
        let (title, artist, album) = match new.track.as_ref() {
            Some(t) => (t.title.clone(), t.artists.join(", "), t.album.clone().unwrap_or_default()),
            None => Default::default(),
        };
        // Only a cover file that exists: the framework can't report a failed load.
        let cover = new
            .track
            .as_ref()
            .and_then(|t| t.art_url.as_deref())
            .filter(|u| u.strip_prefix("file://").is_some_and(|p| std::path::Path::new(p).is_file()))
            .map(escape_file_url);
        let meta = MediaMetadata {
            title: Some(&title),
            artist: Some(&artist),
            album: Some(&album),
            cover_url: cover.as_deref(),
            duration: new.track.as_ref().map(|t| duration(t.length_secs)),
        };
        if let Err(e) = controls.set_metadata(meta) {
            tracing::debug!("now playing metadata failed: {e:?}");
        }
    }
    if (force || old.track != new.track || old.status != new.status)
        && let Err(e) = controls.set_playback(playback(new))
    {
        tracing::debug!("now playing state failed: {e:?}");
    }
}

pub fn subscription() -> Subscription<Message> {
    Subscription::run(worker)
}

fn worker() -> impl Stream<Item = Message> {
    iced::stream::channel(32, async move |mut out: mpsc::Sender<Message>| {
        let (tx, mut rx) = mpsc::unbounded();
        let (ready_tx, ready_rx) = iced::futures::channel::oneshot::channel::<Result<(), String>>();
        DispatchQueue::main().exec_async(move || {
            let result = LIVE.with(|live| {
                let mut live = live.borrow_mut();
                // A restarted subscription sends commands to its own channel.
                let mut controls = match live.take() {
                    Some(mut old) => {
                        let _ = old.controls.detach();
                        old.controls
                    }
                    None => {
                        let config = PlatformConfig {
                            display_name: "Listen",
                            dbus_name: listen_core::config::APP_ID,
                            hwnd: None,
                        };
                        MediaControls::new(config).map_err(|e| format!("{e:?}"))?
                    }
                };
                controls
                    .attach(move |event| {
                        if let Some(cmd) = command(event) {
                            let _ = tx.unbounded_send(cmd);
                        }
                    })
                    .map_err(|e| format!("{e:?}"))?;
                let current = MprisState::default();
                apply(&mut controls, &current, &current, true);
                *live = Some(Live { controls, current });
                Ok(())
            });
            let _ = ready_tx.send(result);
        });
        match ready_rx.await {
            Ok(Ok(())) => {
                let handle = MprisHandle { state: Arc::new(Mutex::new(MprisState::default())) };
                let _ = out.send(Message::Mpris(MprisMsg::Ready(handle))).await;
                while let Some(cmd) = rx.next().await {
                    if out.send(Message::Mpris(MprisMsg::Command(cmd))).await.is_err() {
                        break;
                    }
                }
            }
            Ok(Err(e)) => tracing::warn!("media controls (Now Playing) unavailable: {e}"),
            Err(_) => tracing::warn!("media controls (Now Playing) unavailable"),
        }
        std::future::pending::<()>().await;
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buttons_become_commands() {
        assert!(matches!(command(MediaControlEvent::Toggle), Some(MprisCmd::PlayPause)));
        assert!(matches!(command(MediaControlEvent::Next), Some(MprisCmd::Next)));
        assert!(matches!(
            command(MediaControlEvent::SetPosition(MediaPosition(Duration::from_secs(5)))),
            Some(MprisCmd::SetPosition(p)) if (p - 5.0).abs() < 1e-9
        ));
        assert!(command(MediaControlEvent::Quit).is_none());
    }

    #[test]
    fn file_urls_are_escaped() {
        assert_eq!(
            escape_file_url("file:///Users/a/Library/Application Support/Listen/c.jpg"),
            "file:///Users/a/Library/Application%20Support/Listen/c.jpg"
        );
        assert_eq!(escape_file_url("https://x/y z"), "https://x/y z");
    }
}
