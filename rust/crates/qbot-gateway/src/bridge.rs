//! The live platform connection, as seen by code that wants to call platform actions.
//!
//! One connection serves at a time; a new one replaces the old. Actions are correlated to their
//! responses by an `echo` token. When the connection goes away every pending call fails at once
//! rather than waiting for its timeout.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use crate::wire::ActionResponse;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CallError {
    #[error("no platform connection")]
    Disconnected,
    #[error("the platform did not answer within {0:?}")]
    TimedOut(Duration),
}

struct Live {
    generation: u64,
    out: mpsc::UnboundedSender<String>,
    pending: HashMap<String, oneshot::Sender<ActionResponse>>,
}

#[derive(Default)]
pub struct Bridge {
    live: Mutex<Option<Live>>,
    generation: AtomicU64,
    counter: AtomicU64,
}

impl std::fmt::Debug for Bridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bridge")
            .field("connected", &self.is_connected())
            .finish_non_exhaustive()
    }
}

/// Held by the connection task; detaches on drop so a stale connection cannot unseat a newer one.
#[derive(Debug)]
pub struct Attachment<'a> {
    bridge: &'a Bridge,
    generation: u64,
}

impl Drop for Attachment<'_> {
    fn drop(&mut self) {
        let mut live = self
            .bridge
            .live
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if live
            .as_ref()
            .is_some_and(|l| l.generation == self.generation)
        {
            *live = None;
        }
    }
}

impl Bridge {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_connected(&self) -> bool {
        self.live
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some()
    }

    /// Make `out` the connection actions are written to, replacing any previous one.
    pub fn attach(&self, out: mpsc::UnboundedSender<String>) -> Attachment<'_> {
        let generation = self.generation.fetch_add(1, Ordering::Relaxed) + 1;
        *self.live.lock().unwrap_or_else(PoisonError::into_inner) = Some(Live {
            generation,
            out,
            pending: HashMap::new(),
        });
        Attachment {
            bridge: self,
            generation,
        }
    }

    /// Route an action response to its caller. Responses nobody waits for are dropped.
    pub fn complete(&self, response: ActionResponse) {
        let waiter = self
            .live
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_mut()
            .and_then(|l| l.pending.remove(&response.echo));
        if let Some(tx) = waiter {
            let _ = tx.send(response);
        }
    }

    pub async fn call(
        &self,
        action: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<ActionResponse, CallError> {
        let echo = format!("a{}", self.counter.fetch_add(1, Ordering::Relaxed));
        let frame = json!({ "action": action, "params": params, "echo": echo }).to_string();
        let (tx, rx) = oneshot::channel();
        {
            let mut live = self.live.lock().unwrap_or_else(PoisonError::into_inner);
            let Some(live) = live.as_mut() else {
                return Err(CallError::Disconnected);
            };
            live.pending.insert(echo.clone(), tx);
            if live.out.send(frame).is_err() {
                live.pending.remove(&echo);
                return Err(CallError::Disconnected);
            }
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(response)) => Ok(response),
            // The sender was dropped: the connection ended with this call pending.
            Ok(Err(_)) => Err(CallError::Disconnected),
            Err(_) => {
                if let Some(live) = self
                    .live
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .as_mut()
                {
                    live.pending.remove(&echo);
                }
                Err(CallError::TimedOut(timeout))
            }
        }
    }
}
