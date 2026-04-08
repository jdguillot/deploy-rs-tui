//! Async event stream — wraps crossterm's `EventStream` so the App can
//! `select!` over user input alongside background tasks.

use crossterm::event::{Event as CtEvent, EventStream};
use futures::StreamExt;
use tokio::sync::mpsc;

/// Events surfaced to the App.
#[derive(Debug)]
pub enum AppEvent {
    /// A raw terminal event (key, resize, mouse, ...).
    Term(CtEvent),
    /// Periodic redraw / status refresh tick.
    Tick,
}

/// Spawn a background task that forwards crossterm events plus periodic
/// ticks to a channel.
pub fn spawn() -> mpsc::Receiver<AppEvent> {
    let (tx, rx) = mpsc::channel(64);

    let tx_term = tx.clone();
    tokio::spawn(async move {
        let mut stream = EventStream::new();
        while let Some(ev) = stream.next().await {
            match ev {
                Ok(e) => {
                    if tx_term.send(AppEvent::Term(e)).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let tx_tick = tx;
    tokio::spawn(async move {
        // 120ms is fast enough to drive a smooth braille spinner without
        // burning CPU. Frames only repaint when an event arrives, so the
        // ticker also sets the redraw cadence for animated UI bits.
        let mut ticker = tokio::time::interval(std::time::Duration::from_millis(120));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            if tx_tick.send(AppEvent::Tick).await.is_err() {
                break;
            }
        }
    });

    rx
}
