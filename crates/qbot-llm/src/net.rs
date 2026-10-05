//! How the bot's HTTP clients reach the network. Every outbound client (model providers,
//! embeddings, web search, picture downloads) is built here, so the route is decided in one
//! place and only by configuration: proxy variables in the environment are ignored.

use std::time::Duration;

use crate::LlmError;

/// How long establishing a connection may take: an endpoint that does not answer within this is
/// down, and the caller's retry policy or deadline takes over.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// A client's way out: directly, or through one HTTP(S) proxy.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Route {
    #[default]
    Direct,
    /// An `http://` or `https://` proxy URL; every request of the client goes through it.
    Proxy(String),
}

/// A client builder on `route`, with the connect timeout set. Callers add their own deadlines.
pub fn client_builder(route: &Route) -> Result<reqwest::ClientBuilder, LlmError> {
    let builder = reqwest::Client::builder().connect_timeout(CONNECT_TIMEOUT);
    Ok(match route {
        Route::Direct => builder.no_proxy(),
        Route::Proxy(url) => builder.proxy(
            reqwest::Proxy::all(url)
                .map_err(|e| LlmError::InvalidRequest(format!("proxy: {e}")))?,
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_route_builds_a_client_and_a_bad_proxy_is_an_error() {
        assert!(client_builder(&Route::Direct).is_ok_and(|b| b.build().is_ok()));
        assert!(
            client_builder(&Route::Proxy("http://127.0.0.1:7890".into()))
                .is_ok_and(|b| b.build().is_ok())
        );
        assert!(matches!(
            client_builder(&Route::Proxy("not a url".into())),
            Err(LlmError::InvalidRequest(_))
        ));
    }
}
