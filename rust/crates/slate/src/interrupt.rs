//! A latched, cross-task interrupt: the UI trips it (Esc), the agent observes
//! it between operations and races it against in-flight inference.
//!
//! Built on a `watch` channel so a trip that happens between the agent's
//! observation points is not lost — the change stays latched until consumed.

use std::sync::Mutex;
use tokio::sync::watch;

/// Trips the interrupt; cloneable, held by the UI.
#[derive(Clone)]
pub struct Interrupter {
    tx: watch::Sender<u64>,
}

impl Interrupter {
    /// Signal the agent to abandon the current turn.
    pub fn trip(&self) {
        self.tx.send_modify(|n| *n += 1);
    }
}

/// The agent side: observe and consume interrupts. Created by
/// [`Interrupt::channel`]; the receiver is taken once per session in `run`.
pub struct Interrupt {
    rx: Mutex<Option<watch::Receiver<u64>>>,
}

impl Interrupt {
    /// A fresh interrupt pair: the [`Interrupter`] for the UI, the [`Interrupt`]
    /// for the agent.
    pub fn channel() -> (Interrupter, Interrupt) {
        let (tx, rx) = watch::channel(0);
        (
            Interrupter { tx },
            Interrupt {
                rx: Mutex::new(Some(rx)),
            },
        )
    }

    /// Take the observer for one session (the agent holds it as `&mut`).
    pub fn take_receiver(&self) -> Option<InterruptRx> {
        self.rx
            .lock()
            .expect("interrupt lock")
            .take()
            .map(|rx| InterruptRx { rx })
    }
}

/// The owned, mutable observer used inside one session.
pub struct InterruptRx {
    rx: watch::Receiver<u64>,
}

impl InterruptRx {
    /// Mark "now" as the baseline: interrupts observed before this are cleared.
    pub fn baseline(&mut self) {
        self.rx.borrow_and_update();
    }

    /// Consume a pending interrupt if one arrived since the last observation.
    pub fn tripped(&mut self) -> bool {
        if self.rx.has_changed().unwrap_or(false) {
            self.rx.borrow_and_update();
            true
        } else {
            false
        }
    }

    /// Resolve when the next interrupt arrives (and consume it). Use as a
    /// `select!` branch against in-flight work. A dropped [`Interrupter`]
    /// (closed channel) never resolves — it is not an interrupt.
    pub async fn wait(&mut self) {
        if self.rx.changed().await.is_ok() {
            self.rx.borrow_and_update();
        } else {
            std::future::pending::<()>().await;
        }
    }
}
