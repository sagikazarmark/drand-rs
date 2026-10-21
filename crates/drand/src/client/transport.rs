//! The HTTP leaf under the client: [`HttpTransport`] and its implementations,
//! `ReqwestTransport` (feature `reqwest`) and `TowerTransport` (feature `tower`).
//!
//! Everything above the leaf (relay selection, hedging, timeouts, response
//! classification, verification) is the client's; a transport only sends one
//! request and buffers the response. See the [`client`](super) docs for how
//! `Send` is handled on `wasm32`.

use std::{boxed::Box, fmt, pin::Pin};

use core::future::Future;

use ::http::{Request, Response};
use bytes::Bytes;

#[cfg(feature = "reqwest")]
mod reqwest_transport;
#[cfg(feature = "reqwest")]
pub use reqwest_transport::ReqwestTransport;
#[cfg(feature = "tower")]
mod tower_transport;
#[cfg(feature = "tower")]
pub use tower_transport::{BoxError, TowerTransport};

#[cfg(all(target_arch = "wasm32", target_feature = "atomics"))]
compile_error!(
    "the `client` feature assumes single-threaded wasm (no `atomics`): it marks JS futures `Send`"
);

/// The future [`HttpTransport::send`] returns.
pub type TransportFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Response<Bytes>, TransportError>> + Send + 'a>>;

/// `f` as a `Send` future.
///
/// On native targets `f` must already be `Send`. On `wasm32-unknown-unknown`
/// `f` may hold JS values (a `fetch` promise, a timer): the target is
/// single-threaded, and the wrapper panics if it is ever polled or dropped on
/// another thread. For a transport over a JS API:
///
/// ```ignore
/// fn send(&self, req: Request<Bytes>, limit: usize) -> TransportFuture<'_> {
///     Box::pin(assume_send(async move { /* await JS futures here */ }))
/// }
/// ```
#[cfg(not(target_arch = "wasm32"))]
pub fn assume_send<F: Future + Send>(f: F) -> impl Future<Output = F::Output> + Send {
    f
}

/// `f` as a `Send` future.
///
/// On native targets `f` must already be `Send`. On `wasm32-unknown-unknown`
/// `f` may hold JS values (a `fetch` promise, a timer): the target is
/// single-threaded, and the wrapper panics if it is ever polled or dropped on
/// another thread. For a transport over a JS API:
///
/// ```ignore
/// fn send(&self, req: Request<Bytes>, limit: usize) -> TransportFuture<'_> {
///     Box::pin(assume_send(async move { /* await JS futures here */ }))
/// }
/// ```
#[cfg(target_arch = "wasm32")]
pub fn assume_send<F: Future>(f: F) -> impl Future<Output = F::Output> + Send {
    send_wrapper::SendWrapper::new(f)
}

/// Sends one HTTP request and buffers the response.
///
/// The client builds the request (`GET`, the relay's round URL, its
/// `User-Agent`) and does everything else: hedging across relays, the
/// per-request timeout (it races `send` against its own timer, so a transport
/// may ignore timeouts), response classification and verification.
///
/// A transport must:
///
/// - return non-2xx responses as `Ok`: the client tells 425 from 404 and so on;
/// - buffer at most `limit` bytes of body, failing with
///   [`TransportErrorKind::TooLarge`] beyond that, up front if `Content-Length`
///   says so and while reading otherwise (a relay must not be able to make the
///   client allocate without bound). The limit applies whatever the status:
///   an oversized error page is reported as too large, not by its status.
///
/// `ReqwestTransport` (feature `reqwest`) is the default; `TowerTransport`
/// (feature `tower`) adapts any `tower::Service` over `http` types, such as
/// `tower-reqwest` or, on Cloudflare Workers, `tower-worker-fetch`. Implement
/// this for anything else (a JS host's `fetch` directly, an in-memory fake in
/// tests). On `wasm32`, see [`assume_send`].
pub trait HttpTransport: Send + Sync + 'static {
    /// Sends `req`, buffering at most `limit` bytes of body.
    fn send(&self, req: Request<Bytes>, limit: usize) -> TransportFuture<'_>;
}

/// A shared transport (also `Arc<dyn HttpTransport>`).
impl<T: HttpTransport + ?Sized> HttpTransport for std::sync::Arc<T> {
    fn send(&self, req: Request<Bytes>, limit: usize) -> TransportFuture<'_> {
        (**self).send(req, limit)
    }
}

/// What went wrong in a transport.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TransportErrorKind {
    /// The transport's own timeout fired (the client has one above it too).
    Timeout,
    /// No connection: DNS, TCP, TLS.
    Connect,
    /// The response body could not be read.
    Body,
    /// The response body exceeds the limit.
    TooLarge,
    /// Anything else.
    Other,
}

impl TransportErrorKind {
    fn message(self) -> &'static str {
        match self {
            Self::Timeout => "timed out",
            Self::Connect => "connection failed",
            Self::Body => "reading the response body failed",
            Self::TooLarge => "response too large",
            Self::Other => "transport error",
        }
    }
}

/// A transport failure: a [`TransportErrorKind`] and an optional cause.
#[non_exhaustive]
#[derive(Debug)]
pub struct TransportError {
    kind: TransportErrorKind,
    source: Option<Box<dyn core::error::Error + Send + Sync>>,
}

impl TransportError {
    /// An error of `kind` without a cause.
    #[must_use]
    pub fn new(kind: TransportErrorKind) -> Self {
        Self { kind, source: None }
    }

    /// An error of `kind` caused by `source`.
    #[must_use]
    pub fn with_source(
        kind: TransportErrorKind,
        source: impl Into<Box<dyn core::error::Error + Send + Sync>>,
    ) -> Self {
        Self {
            kind,
            source: Some(source.into()),
        }
    }

    /// What went wrong.
    #[must_use]
    pub fn kind(&self) -> TransportErrorKind {
        self.kind
    }
}

impl fmt::Display for TransportError {
    /// The kind, and the cause if there is one (`connection failed: refused`).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.kind.message())?;
        if let Some(source) = &self.source {
            write!(f, ": {source}")?;
        }
        Ok(())
    }
}

impl core::error::Error for TransportError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        self.source
            .as_deref()
            .map(|e| e as &(dyn core::error::Error + 'static))
    }
}
