//! Matching a sent message to the platform's report of it.
//!
//! The platform reports every message the bot sends as an event of its own. That report is the
//! only place a nondeterministic result (a dice roll) exists, so a send is complete only when
//! its echo has been archived. The echo may arrive before or after the action response that
//! names the message, so either side may come first.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use qbot_context::ChatLine;
use qbot_core::{GroupId, MessageId};
use tokio::sync::oneshot;
use tokio::time::Instant;

enum Slot {
    Waiting(oneshot::Sender<ChatLine>),
    Arrived(ChatLine, Instant),
}

#[derive(Default)]
pub struct EchoBoard {
    slots: Mutex<HashMap<(GroupId, MessageId), Slot>>,
}

impl std::fmt::Debug for EchoBoard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EchoBoard").finish_non_exhaustive()
    }
}

impl EchoBoard {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an archived bot line. Lines nobody waits for are kept for `keep` in case the
    /// action response is still on its way, then dropped, so the board stays bounded by the
    /// echo timeout rather than by uptime.
    pub fn publish(&self, group: GroupId, message: MessageId, line: ChatLine, keep: Duration) {
        let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        let now = Instant::now();
        slots.retain(|_, slot| match slot {
            Slot::Arrived(_, at) => now.duration_since(*at) < keep,
            Slot::Waiting(_) => true,
        });
        match slots.remove(&(group, message)) {
            Some(Slot::Waiting(tx)) => {
                // A closed receiver means the waiter timed out; the line is already archived.
                let _ = tx.send(line);
            }
            _ => {
                slots.insert((group, message), Slot::Arrived(line, now));
            }
        }
    }

    /// Wait for the echo of `message`, or `None` after `timeout`.
    pub async fn wait(
        &self,
        group: GroupId,
        message: MessageId,
        timeout: Duration,
    ) -> Option<ChatLine> {
        let rx = {
            let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(Slot::Arrived(line, _)) = slots.remove(&(group, message)) {
                return Some(line);
            }
            let (tx, rx) = oneshot::channel();
            slots.insert((group, message), Slot::Waiting(tx));
            rx
        };
        let outcome = tokio::time::timeout(timeout, rx).await;
        if !matches!(outcome, Ok(Ok(_))) {
            self.slots
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&(group, message));
        }
        outcome.ok().and_then(Result::ok)
    }
}
