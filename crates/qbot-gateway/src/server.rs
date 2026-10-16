//! The reverse WebSocket endpoint the platform connects to.

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures_util::{SinkExt, StreamExt};
use qbot_core::AccountId;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::bridge::Bridge;
use crate::pipeline::Pipeline;
use crate::wire::{Frame, parse_frame};

/// Where NapCat's reverse WebSocket connects: the path OneBot v11 implementations use.
pub const PATH: &str = "/onebot/v11/ws";

#[derive(Clone)]
pub struct GatewayServer {
    pub pipeline: Arc<Pipeline>,
    pub bridge: Arc<Bridge>,
    pub bot: AccountId,
    /// When set, connections must present it as a bearer token or an `access_token` query value.
    pub access_token: Option<Arc<str>>,
    pub path: Arc<str>,
    /// Ends open connections on shutdown.
    pub cancel: CancellationToken,
}

impl std::fmt::Debug for GatewayServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatewayServer")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

/// Compare secrets without an early exit on the first differing byte.
fn same_secret(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

impl GatewayServer {
    fn authorized(&self, headers: &HeaderMap, query: &HashMap<String, String>) -> bool {
        let Some(expected) = &self.access_token else {
            return true;
        };
        let bearer = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| {
                v.strip_prefix("Bearer ")
                    .or_else(|| v.strip_prefix("bearer "))
            });
        let presented = bearer.or_else(|| query.get("access_token").map(String::as_str));
        presented.is_some_and(|p| same_secret(p, expected))
    }

    pub fn router(self) -> Router {
        let path = self.path.to_string();
        Router::new().route(&path, get(upgrade)).with_state(self)
    }
}

async fn upgrade(
    State(server): State<GatewayServer>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    ws: WebSocketUpgrade,
) -> Response {
    if !server.authorized(&headers, &query) {
        tracing::warn!("connection refused: bad or missing access token");
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if let Some(claimed) = headers.get("x-self-id").and_then(|v| v.to_str().ok())
        && claimed.trim() != server.bot.get().to_string()
    {
        tracing::warn!(
            claimed,
            "connection refused: it is for a different bot account"
        );
        return StatusCode::FORBIDDEN.into_response();
    }
    ws.on_upgrade(move |socket| connection(server, socket))
}

async fn connection(server: GatewayServer, socket: WebSocket) {
    tracing::info!("platform connected");
    let (mut sink, mut stream) = socket.split();
    let (out, mut outgoing) = mpsc::unbounded_channel::<String>();
    let _attached = server.bridge.attach(out);
    let writer = tokio::spawn(async move {
        while let Some(frame) = outgoing.recv().await {
            if sink.send(Message::Text(frame.into())).await.is_err() {
                break;
            }
        }
    });
    loop {
        let received = tokio::select! {
            () = server.cancel.cancelled() => break,
            next = stream.next() => match next {
                Some(received) => received,
                None => break,
            },
        };
        let text = match received {
            Ok(Message::Text(text)) => text,
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(_) => continue,
        };
        match parse_frame(&text) {
            Frame::Response(response) => server.bridge.complete(response),
            Frame::Ignored(reason) => tracing::trace!(reason, "frame ignored"),
            frame => server.pipeline.handle(frame).await,
        }
    }
    writer.abort();
    tracing::info!("platform disconnected");
}

/// How long the platform may be away before it is worth a warning: a restart of NapCat or a
/// reconnect takes seconds, an expired QQ session lasts until someone logs in again.
pub const DISCONNECTED_WARN_AFTER: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// Warn while no platform connection is attached: after `after`, then every hour, and say when
/// it is back. Without it, a lasting outage (an expired QQ session, NapCat down) leaves one INFO
/// line and nothing else, while the bot hears and says nothing.
pub async fn warn_while_disconnected(
    bridge: Arc<Bridge>,
    after: std::time::Duration,
    cancel: CancellationToken,
) {
    let check = (after / 10).max(std::time::Duration::from_millis(10));
    let mut since = (!bridge.is_connected()).then(tokio::time::Instant::now);
    let mut next_warning = after;
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(check) => {}
        }
        if bridge.is_connected() {
            if let Some(down) = since.take()
                && down.elapsed() >= after
            {
                tracing::info!(
                    minutes = down.elapsed().as_secs() / 60,
                    "the platform is connected again"
                );
            }
            next_warning = after;
            continue;
        }
        let down = *since.get_or_insert_with(tokio::time::Instant::now);
        if down.elapsed() >= next_warning {
            tracing::warn!(
                minutes = down.elapsed().as_secs() / 60,
                "no platform connection: the bot hears and says nothing until NapCat reconnects"
            );
            next_warning += std::time::Duration::from_secs(3600);
        }
    }
}

/// Serve until `cancel` fires. Open connections are closed by the shutdown.
pub async fn serve(
    listener: TcpListener,
    server: GatewayServer,
    cancel: CancellationToken,
) -> std::io::Result<()> {
    axum::serve(listener, server.router())
        .with_graceful_shutdown(async move { cancel.cancelled().await })
        .await
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use tokio_util::sync::CancellationToken;
    use tracing_subscriber::layer::SubscriberExt;

    use super::*;

    /// Collects the warnings logged while the guard is held.
    #[derive(Clone, Default)]
    struct Warnings(Arc<Mutex<Vec<String>>>);

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Warnings {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if *event.metadata().level() == tracing::Level::WARN {
                self.0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(event.metadata().name().to_owned());
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_lasting_outage_is_warned_about_after_ten_minutes_then_hourly() {
        let warnings = Warnings::default();
        let _guard =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(warnings.clone()));
        let bridge = Arc::new(Bridge::new());
        let cancel = CancellationToken::new();
        let watch = tokio::spawn(warn_while_disconnected(
            bridge.clone(),
            DISCONNECTED_WARN_AFTER,
            cancel.clone(),
        ));
        let count = || {
            warnings
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len()
        };
        tokio::time::sleep(Duration::from_secs(9 * 60)).await;
        assert_eq!(count(), 0, "a short gap is not worth a warning");
        tokio::time::sleep(Duration::from_secs(2 * 60)).await;
        assert_eq!(count(), 1, "warned after ten minutes");
        tokio::time::sleep(Duration::from_secs(30 * 60)).await;
        assert_eq!(count(), 1, "not again within the hour");
        tokio::time::sleep(Duration::from_secs(40 * 60)).await;
        assert_eq!(count(), 2, "then hourly");
        // Connected: no more warnings.
        let (out, _rx) = tokio::sync::mpsc::unbounded_channel();
        let _attached = bridge.attach(out);
        tokio::time::sleep(Duration::from_secs(3 * 3600)).await;
        assert_eq!(count(), 2);
        cancel.cancel();
        watch.await.unwrap();
    }
}
