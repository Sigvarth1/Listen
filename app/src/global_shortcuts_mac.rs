//! macOS system-wide shortcuts (`global-hotkey`: Carbon hot keys, no Accessibility permission
//! needed), with the interface of the Linux portal module. The media keys need nothing here: they
//! reach Listen through Now Playing.

use crate::app::Message;
use dispatch2::DispatchQueue;
use global_hotkey::hotkey::HotKey;
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use iced::Subscription;
use iced::futures::{SinkExt, Stream, StreamExt, channel::mpsc};
use std::cell::RefCell;

#[derive(Debug, Clone)]
pub enum ShortcutMsg {
    /// The shortcuts were registered; (action, key as shown).
    Bound(Vec<(String, String)>),
    /// A registered shortcut fired (action id).
    Activated(String),
    /// Some shortcut could not be registered (another app owns the key).
    Unavailable(String),
}

/// (action, label, binding in Feishin's notation) for every shortcut we register.
pub type Wanted = Vec<(String, String, String)>;

/// macOS routes the hardware media keys to Now Playing; nothing to grab.
pub const MEDIA_KEYS: &[(&str, &str, &str, &str)] = &[];

/// Every bound action flagged global.
pub fn wanted(settings: &listen_core::settings::Settings, _desktop_has_media_keys: bool) -> Wanted {
    listen_core::settings::HOTKEY_ACTIONS
        .iter()
        .filter(|a| a.allow_global && settings.hotkeys_global.contains(a.id))
        .filter_map(|a| {
            let binding = settings.hotkeys.get(a.id).filter(|h| crate::hotkeys::parse(h).is_some())?;
            Some((a.id.to_string(), a.label.to_string(), binding.clone()))
        })
        .collect()
}

/// The media keys always go through Now Playing on macOS.
pub fn session_handles_media_keys() -> bool {
    true
}

pub fn action_of(shortcut_id: &str) -> &str {
    shortcut_id
}

/// A binding (`mod+alt+arrowright`) as a system hot key: `mod` is Cmd and `meta` Control on macOS.
pub fn hotkey(binding: &str) -> Option<HotKey> {
    let b = crate::hotkeys::parse(binding)?;
    let mut parts: Vec<&str> = Vec::new();
    if b.ctrl {
        parts.push("super");
    }
    if b.meta {
        parts.push("control");
    }
    if b.alt {
        parts.push("alt");
    }
    if b.shift {
        parts.push("shift");
    }
    parts.push(&b.key);
    parts.join("+").parse().ok()
}

/// The manager and what is registered; lives on the main thread.
struct Live {
    manager: GlobalHotKeyManager,
    registered: Vec<HotKey>,
    /// The subscription that registered them.
    generation: u64,
}

thread_local! {
    static LIVE: RefCell<Option<Live>> = const { RefCell::new(None) };
}

/// Where key presses go: (hot key id, action) of the current subscription and its channel.
/// The event handler can only be set once per process, so it reads this.
type Routes = (Vec<(u32, String)>, Option<mpsc::UnboundedSender<ShortcutMsg>>);
static ROUTES: std::sync::Mutex<Routes> = std::sync::Mutex::new((Vec::new(), None));
static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn install_handler() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        GlobalHotKeyEvent::set_event_handler(Some(|event: GlobalHotKeyEvent| {
            if event.state != HotKeyState::Pressed {
                return;
            }
            if let Ok(routes) = ROUTES.lock()
                && let (ids, Some(tx)) = &*routes
                && let Some((_, action)) = ids.iter().find(|(id, _)| *id == event.id)
            {
                let _ = tx.unbounded_send(ShortcutMsg::Activated(action.clone()));
            }
        }));
    });
}

/// Unregisters this subscription's keys when it ends (unless a newer one took over).
struct Unregister(u64);

impl Drop for Unregister {
    fn drop(&mut self) {
        let generation = self.0;
        DispatchQueue::main().exec_async(move || {
            LIVE.with(|live| {
                if let Some(live) = live.borrow_mut().as_mut()
                    && live.generation == generation
                {
                    let _ = live.manager.unregister_all(&live.registered);
                    live.registered.clear();
                }
            });
        });
    }
}

pub fn subscription(wanted: Wanted) -> Subscription<Message> {
    Subscription::run_with(wanted, worker)
}

fn worker(wanted: &Wanted) -> impl Stream<Item = Message> + use<> {
    let wanted = wanted.clone();
    iced::stream::channel(16, async move |mut out: mpsc::Sender<Message>| {
        let (tx, mut rx) = mpsc::unbounded::<ShortcutMsg>();
        let generation = GENERATION.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        let _unregister = Unregister(generation);
        let keys: Vec<(String, String, Option<HotKey>)> =
            wanted.iter().map(|(id, _, binding)| (id.clone(), binding.clone(), hotkey(binding))).collect();
        if let Ok(mut routes) = ROUTES.lock() {
            routes.0 = keys.iter().filter_map(|(id, _, k)| Some((k.as_ref()?.id(), id.clone()))).collect();
            routes.1 = Some(tx.clone());
        }
        install_handler();
        DispatchQueue::main().exec_async(move || {
            LIVE.with(|live| {
                let mut live = live.borrow_mut();
                if live.is_none() {
                    match GlobalHotKeyManager::new() {
                        Ok(manager) => *live = Some(Live { manager, registered: Vec::new(), generation }),
                        Err(e) => {
                            let _ = tx.unbounded_send(ShortcutMsg::Unavailable(e.to_string()));
                            return;
                        }
                    }
                }
                let Some(live) = live.as_mut() else { return };
                // The previous set (this subscription replaces the last one).
                let _ = live.manager.unregister_all(&live.registered);
                live.registered.clear();
                live.generation = generation;
                let (mut bound, mut failed) = (Vec::new(), Vec::new());
                for (id, binding, key) in keys {
                    let shown = crate::ui::tip::binding_text(&binding).unwrap_or(binding);
                    match key.map(|k| (k, live.manager.register(k))) {
                        Some((k, Ok(()))) => {
                            live.registered.push(k);
                            bound.push((id, shown));
                        }
                        Some((_, Err(e))) => failed.push(format!("{shown} ({e})")),
                        None => failed.push(format!("{shown} (not a key macOS can register)")),
                    }
                }
                let _ = tx.unbounded_send(ShortcutMsg::Bound(bound));
                if !failed.is_empty() {
                    let _ = tx.unbounded_send(ShortcutMsg::Unavailable(failed.join(", ")));
                }
            });
        });
        while let Some(msg) = rx.next().await {
            if out.send(Message::GlobalShortcut(msg)).await.is_err() {
                break;
            }
        }
        std::future::pending::<()>().await;
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use global_hotkey::hotkey::{Code, Modifiers};
    use listen_core::settings::Settings;

    #[test]
    fn bindings_become_mac_hot_keys() {
        let k = hotkey("mod+alt+arrowright").unwrap();
        assert_eq!((k.mods, k.key), (Modifiers::SUPER | Modifiers::ALT, Code::ArrowRight));
        let k = hotkey("meta+shift+k").unwrap();
        assert_eq!((k.mods, k.key), (Modifiers::CONTROL | Modifiers::SHIFT, Code::KeyK));
        assert_eq!(hotkey("f5").unwrap().key, Code::F5);
        assert!(hotkey("").is_none());
    }

    #[test]
    fn registers_bound_global_actions_only() {
        let mut s = Settings { global_media_keys: true, ..Settings::default() };
        assert!(wanted(&s, false).is_empty(), "no media keys to grab on macOS");
        s.hotkeys.insert("next".into(), "mod+alt+arrowright".into());
        s.hotkeys_global.insert("next".into());
        s.hotkeys_global.insert("stop".into()); // flagged but unbound
        s.hotkeys_global.insert("globalSearch".into()); // never global
        assert_eq!(
            wanted(&s, false),
            vec![("next".into(), "Next track".into(), "mod+alt+arrowright".into())]
        );
    }
}
