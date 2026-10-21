//! [`ReqwestTransport`]: the default [`HttpTransport`], over a [`reqwest::Client`].

use std::{boxed::Box, vec::Vec};

use ::http::{Request, Response};
use bytes::Bytes;
use futures_util::StreamExt;

use super::{HttpTransport, TransportError, TransportErrorKind, TransportFuture, assume_send};

/// The default [`HttpTransport`]: a [`reqwest::Client`].
///
/// This is the only place `reqwest` shows in the crate's API. Bodies are read
/// in chunks and capped at the limit. `ReqwestTransport::try_default()` and
/// `Default` exist only with a TLS feature (`default-tls`, `rustls`,
/// `native-tls`); without one, bring a configured client to
/// [`ReqwestTransport::new`].
#[derive(Clone, Debug)]
pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl ReqwestTransport {
    /// Over `client` (e.g. one configured with a proxy or custom TLS roots).
    #[must_use]
    pub fn new(client: reqwest::Client) -> Self {
        Self { client }
    }

    /// Over a client with reqwest's defaults for this crate's feature set.
    ///
    /// # Errors
    ///
    /// [`TransportErrorKind::Other`] if reqwest cannot build a client (its TLS
    /// backend failed to initialize).
    #[cfg(any(feature = "default-tls", feature = "rustls", feature = "native-tls"))]
    pub fn try_default() -> Result<Self, TransportError> {
        reqwest::Client::builder()
            .build()
            .map(Self::new)
            .map_err(|e| TransportError::with_source(TransportErrorKind::Other, e))
    }
}

#[cfg(any(feature = "default-tls", feature = "rustls", feature = "native-tls"))]
impl Default for ReqwestTransport {
    /// Like [`ReqwestTransport::try_default`].
    ///
    /// # Panics
    ///
    /// If reqwest cannot build a client (as [`reqwest::Client::new`] does).
    fn default() -> Self {
        Self::try_default().expect("reqwest::Client::builder().build() failed")
    }
}

impl HttpTransport for ReqwestTransport {
    fn send(&self, req: Request<Bytes>, limit: usize) -> TransportFuture<'_> {
        // On wasm, reqwest's future awaits JS promises: see `assume_send`.
        Box::pin(assume_send(send(self.client.clone(), req, limit)))
    }
}

async fn send(
    client: reqwest::Client,
    req: Request<Bytes>,
    limit: usize,
) -> Result<Response<Bytes>, TransportError> {
    let req = reqwest::Request::try_from(req).map_err(classify)?;
    let mut resp = client.execute(req).await.map_err(classify)?;
    if resp.content_length().is_some_and(|n| n > limit as u64) {
        return Err(TransportError::new(TransportErrorKind::TooLarge));
    }
    let status = resp.status();
    let headers = core::mem::take(resp.headers_mut());
    let mut body = Vec::new();
    let mut chunks = core::pin::pin!(chunks(resp));
    while let Some(chunk) = chunks.next().await.transpose().map_err(classify)? {
        if body.len() + chunk.len() > limit {
            return Err(TransportError::new(TransportErrorKind::TooLarge));
        }
        body.extend_from_slice(&chunk);
    }
    let mut out = Response::new(Bytes::from(body));
    *out.status_mut() = status;
    *out.headers_mut() = headers;
    Ok(out)
}

fn classify(e: reqwest::Error) -> TransportError {
    let kind = if e.is_timeout() {
        TransportErrorKind::Timeout
    } else if is_connect(&e) {
        TransportErrorKind::Connect
    } else if e.is_body() || e.is_decode() {
        TransportErrorKind::Body
    } else {
        TransportErrorKind::Other
    };
    TransportError::with_source(kind, e)
}

#[cfg(not(target_arch = "wasm32"))]
fn is_connect(e: &reqwest::Error) -> bool {
    e.is_connect()
}

/// `fetch` does not tell connection failures apart.
#[cfg(target_arch = "wasm32")]
fn is_connect(_: &reqwest::Error) -> bool {
    false
}

/// The body in chunks: `chunk()` on native, `bytes_stream()` on wasm (where
/// `chunk()` does not exist).
#[cfg(not(target_arch = "wasm32"))]
fn chunks(resp: reqwest::Response) -> impl futures_util::Stream<Item = reqwest::Result<Bytes>> {
    futures_util::stream::unfold(resp, |mut resp| async move {
        resp.chunk().await.transpose().map(|chunk| (chunk, resp))
    })
}

/// The body in chunks: `chunk()` on native, `bytes_stream()` on wasm (where
/// `chunk()` does not exist).
#[cfg(target_arch = "wasm32")]
fn chunks(resp: reqwest::Response) -> impl futures_util::Stream<Item = reqwest::Result<Bytes>> {
    resp.bytes_stream()
}

#[allow(dead_code)]
fn assert_send_sync(transport: &ReqwestTransport) {
    fn send<T: Send>(_: T) {}
    fn send_sync<T: Send + Sync>(_: T) {}
    send_sync(transport.clone());
    send(transport.send(Request::new(Bytes::new()), 0));
}
