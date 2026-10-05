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
