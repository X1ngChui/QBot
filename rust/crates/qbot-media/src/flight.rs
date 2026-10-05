//! One piece of work per key at a time: callers that arrive while it runs share its answer.
//!
//! A description takes seconds, and the arrival pass, a forwarded copy and a repost can all want
//! the same picture inside that window; without this each would pay for its own call.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};

use futures_util::FutureExt;
use futures_util::future::{BoxFuture, Shared};

type Flight = Shared<BoxFuture<'static, Option<String>>>;

#[derive(Default)]
pub struct Flights {
    running: Mutex<HashMap<String, Flight>>,
}

impl std::fmt::Debug for Flights {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Flights").finish_non_exhaustive()
    }
}

impl Flights {
    /// Run `work` for `key`, or join the run already in the air.
    pub async fn run<F>(self: &Arc<Self>, key: String, work: F) -> Option<String>
    where
        F: Future<Output = Option<String>> + Send + 'static,
    {
        let flight = {
            let mut running = self.running.lock().unwrap_or_else(PoisonError::into_inner);
            match running.get(&key) {
                Some(existing) => existing.clone(),
                None => {
                    let flights = Arc::clone(self);
                    let owned = key.clone();
                    let flight: Flight = async move {
                        let answer = work.await;
                        flights
                            .running
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .remove(&owned);
                        answer
                    }
                    .boxed()
                    .shared();
                    running.insert(key, flight.clone());
                    flight
                }
            }
        };
        flight.await
    }
}
