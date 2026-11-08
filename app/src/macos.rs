//! macOS desktop glue that winit doesn't cover: a click on the Dock icon (the app becoming
//! active) brings back a window that was hidden to the menu bar.

use crate::app::Message;
use block2::RcBlock;
use iced::Subscription;
use iced::futures::{SinkExt, Stream, StreamExt, channel::mpsc};
use objc2_foundation::{NSNotification, NSNotificationCenter, NSString};
use std::ptr::NonNull;

/// Messages each time Listen becomes the active app (Dock icon, app switcher, Finder).
pub fn activations() -> Subscription<Message> {
    Subscription::run(worker)
}

fn worker() -> impl Stream<Item = Message> {
    iced::stream::channel(4, async move |mut out: mpsc::Sender<Message>| {
        let (tx, mut rx) = mpsc::unbounded::<()>();
        dispatch2::DispatchQueue::main().exec_async(move || {
            let block = RcBlock::new(move |_: NonNull<NSNotification>| {
                let _ = tx.unbounded_send(());
            });
            let name = NSString::from_str("NSApplicationDidBecomeActiveNotification");
            // SAFETY: a plain observer on the main queue; the block only sends on a channel.
            let observer = unsafe {
                NSNotificationCenter::defaultCenter().addObserverForName_object_queue_usingBlock(
                    Some(&name),
                    None,
                    None,
                    &block,
                )
            };
            // Observes for the rest of the run (the subscription never ends while Listen runs).
            std::mem::forget(observer);
        });
        while rx.next().await.is_some() {
            if out.send(Message::AppActivated).await.is_err() {
                break;
            }
        }
        std::future::pending::<()>().await;
    })
}
