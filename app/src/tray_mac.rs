//! macOS menu bar icon (`tray-icon`), same shape as the Linux `tray` module: Feishin's menu
//! (play/pause, next, previous, stop, open, quit). AppKit objects live on the main thread, so the
//! icon is built and updated through the main dispatch queue; the app talks to it through a handle.

use crate::app::Message;
use dispatch2::DispatchQueue;
use iced::Subscription;
use iced::futures::{SinkExt, Stream, StreamExt, channel::mpsc};
use std::cell::RefCell;
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayCmd {
    PlayPause,
    Next,
    Previous,
    Stop,
    /// "Open main window".
    Show,
    /// A click on the icon (macOS opens the menu instead).
    Toggle,
    Quit,
}

/// The menu's labels in order (`None` = separator).
pub fn menu_labels(playing: bool) -> Vec<Option<&'static str>> {
    vec![
        Some(if playing { "Pause" } else { "Play" }),
        Some("Next Track"),
        Some("Previous Track"),
        Some("Stop"),
        None,
        Some("Open main window"),
        Some("Quit"),
    ]
}

const COMMANDS: [Option<TrayCmd>; 7] = [
    Some(TrayCmd::PlayPause),
    Some(TrayCmd::Next),
    Some(TrayCmd::Previous),
    Some(TrayCmd::Stop),
    None,
    Some(TrayCmd::Show),
    Some(TrayCmd::Quit),
];

/// The 64 px app icon (rendered from `packaging/io.github.leks2205.Listen.svg`).
const ICON_PNG: &[u8] = include_bytes!("../../packaging/windows/tray-64.png");

fn icon() -> Option<tray_icon::Icon> {
    let img = image::load_from_memory_with_format(ICON_PNG, image::ImageFormat::Png).ok()?.into_rgba8();
    let (w, h) = img.dimensions();
    tray_icon::Icon::from_rgba(img.into_raw(), w, h).ok()
}

/// The icon and its play/pause item; lives on the main thread.
struct Live {
    tray: TrayIcon,
    play: MenuItem,
}

thread_local! {
    static LIVE: RefCell<Option<Live>> = const { RefCell::new(None) };
}

/// Handle for updating the tray from the app.
#[derive(Clone)]
pub struct TrayHandle;

impl std::fmt::Debug for TrayHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TrayHandle")
    }
}

impl TrayHandle {
    pub async fn set(self, playing: bool, title: String) {
        DispatchQueue::main().exec_async(move || {
            LIVE.with(|live| {
                if let Some(live) = live.borrow().as_ref() {
                    live.play.set_text(menu_labels(playing)[0].unwrap_or("Play"));
                    let tip =
                        if title.is_empty() { "Listen".to_string() } else { format!("Listen\n{title}") };
                    let _ = live.tray.set_tooltip(Some(tip));
                }
            });
        });
    }
}

#[derive(Debug, Clone)]
pub enum TrayMsg {
    Ready(TrayHandle),
    Command(TrayCmd),
    /// The tray icon couldn't be created.
    Unavailable,
}

/// Builds the icon (main thread). Menu clicks go to `commands`.
fn build(commands: mpsc::UnboundedSender<TrayCmd>) -> Result<Live, String> {
    let menu = Menu::new();
    let mut play = None;
    let mut ids = Vec::new();
    for (label, cmd) in menu_labels(false).into_iter().zip(COMMANDS) {
        match (label, cmd) {
            (Some(label), Some(cmd)) => {
                let item = MenuItem::new(label, true, None);
                menu.append(&item).map_err(|e| e.to_string())?;
                ids.push((item.id().clone(), cmd));
                if cmd == TrayCmd::PlayPause {
                    play = Some(item);
                }
            }
            _ => menu.append(&PredefinedMenuItem::separator()).map_err(|e| e.to_string())?,
        }
    }
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        if let Some((_, cmd)) = ids.iter().find(|(id, _)| *id == event.id) {
            let _ = commands.unbounded_send(*cmd);
        }
    }));
    let mut builder =
        TrayIconBuilder::new().with_menu(Box::new(menu)).with_menu_on_left_click(true).with_tooltip("Listen");
    if let Some(icon) = icon() {
        builder = builder.with_icon(icon);
    }
    let tray = builder.build().map_err(|e| e.to_string())?;
    Ok(Live { tray, play: play.ok_or("menu without play item")? })
}

pub fn subscription() -> Subscription<Message> {
    Subscription::run(worker)
}

fn worker() -> impl Stream<Item = Message> {
    iced::stream::channel(16, async move |mut out: mpsc::Sender<Message>| {
        let (tx, mut rx) = mpsc::unbounded();
        let (ready_tx, ready_rx) = iced::futures::channel::oneshot::channel::<Result<(), String>>();
        DispatchQueue::main().exec_async(move || {
            let result = LIVE.with(|live| {
                // A restarted subscription (tray switched off and on) builds a fresh icon.
                drop(live.borrow_mut().take());
                *live.borrow_mut() = Some(build(tx)?);
                Ok(())
            });
            let _ = ready_tx.send(result);
        });
        // Removing the icon when the subscription ends (tray switched off).
        struct Remove;
        impl Drop for Remove {
            fn drop(&mut self) {
                DispatchQueue::main().exec_async(|| LIVE.with(|live| drop(live.borrow_mut().take())));
            }
        }
        let _remove = Remove;
        match ready_rx.await {
            Ok(Ok(())) => {
                let _ = out.send(Message::Tray(TrayMsg::Ready(TrayHandle))).await;
                while let Some(cmd) = rx.next().await {
                    if out.send(Message::Tray(TrayMsg::Command(cmd))).await.is_err() {
                        break;
                    }
                }
            }
            other => {
                tracing::info!("menu bar icon unavailable: {:?}", other.ok().and_then(Result::err));
                let _ = out.send(Message::Tray(TrayMsg::Unavailable)).await;
            }
        }
        std::future::pending::<()>().await;
    })
}
