//! [`TowerTransport`]: an [`HttpTransport`] over a [`tower_service::Service`].

use std::{boxed::Box, fmt, vec::Vec};

use core::{future::poll_fn, pin::pin};

use ::http::{HeaderMap, Request, Response, header::CONTENT_LENGTH};
use bytes::Bytes;
use http_body::Body;
use http_body_util::{BodyExt, Full};
use tower_service::Service;

use super::{HttpTransport, TransportError, TransportErrorKind, TransportFuture};

/// A boxed error, as tower services return them.
pub type BoxError = Box<dyn core::error::Error + Send + Sync>;

/// An [`HttpTransport`] over any [`tower_service::Service`] that takes an
/// `http::Request<Full<Bytes>>` and returns an `http::Response` with a
/// streaming [`http_body::Body`]: `tower-worker-fetch` on Cloudflare Workers
/// as is (it accepts any `http_body::Body`), `tower-reqwest` behind a
/// `map_request` that converts the body (it takes `Request<reqwest::Body>`;
/// `req.map(reqwest::Body::wrap)`), or your own stack of layers over either.
///
/// Each `send` clones the service and drives it once (`poll_ready`, then
/// `call`), the usual tower pattern: services are expected to be cheap to
/// clone. The response body is read frame by frame and capped at the client's
/// limit, up front when `Content-Length` (or the body's own size hint) says it
/// is too large, and while reading otherwise, so a relay cannot make the client
/// buffer without bound.
///
/// Errors from the service map to [`TransportErrorKind::Other`] and errors
/// while reading the body to [`TransportErrorKind::Body`], both keeping the
/// original error as the source. [`TowerTransport::with_classifier`] refines
/// that for a service whose error type says more.
///
/// The service, its future and its body must be `Send` (and the service
/// `Sync`), on `wasm32` too: this adapter does not wrap JS futures. Services
/// built for Workers (`tower-worker-fetch`) already are. The client enforces
/// the per-request timeout above the transport, so no `tower::timeout` layer
/// is needed (and on Workers, where there is no tokio, it would panic).
///
/// ```ignore
/// use drand::client::{Client, TowerTransport};
/// use tower_worker_fetch::FetchService;
///
/// let transport = TowerTransport::new(FetchService::new());
/// let client = Client::builder(drand::chains::quicknet())
///     .http_transport(transport)
///     .build()?;
/// ```
#[derive(Clone)]
pub struct TowerTransport<S> {
    service: S,
    classify: Option<Classifier>,
}

/// See [`TowerTransport::with_classifier`].
type Classifier = fn(&BoxError) -> Option<TransportErrorKind>;

impl<S> TowerTransport<S> {
    /// Over `service`.
    #[must_use]
    pub fn new(service: S) -> Self {
        Self {
            service,
            classify: None,
        }
    }

    /// Classifies the service's (and its body's) errors.
    ///
    /// `classify` sees each error boxed, as the service returned it, and
    /// returns the [`TransportErrorKind`] to report, or `None` for the default
    /// ([`TransportErrorKind::Other`] from the service,
    /// [`TransportErrorKind::Body`] while reading the body). Downcast to the
    /// service's error type to tell its timeouts and connection failures apart:
    ///
    /// ```ignore
    /// use drand::client::{BoxError, TowerTransport, TransportErrorKind};
    ///
    /// let transport = TowerTransport::new(service).with_classifier(|e: &BoxError| {
    ///     let e = e.downcast_ref::<tower_worker_fetch::Error>()?;
    ///     Some(if e.is_timeout() {
    ///         TransportErrorKind::Timeout
    ///     } else if e.stage() == tower_worker_fetch::Stage::Body {
    ///         TransportErrorKind::Body
    ///     } else {
    ///         TransportErrorKind::Other
    ///     })
    /// });
    /// ```
    ///
    /// Only diagnostics depend on it: the client retries on the same
    /// conditions whatever the kind.
    #[must_use]
    pub fn with_classifier(mut self, classify: Classifier) -> Self {
        self.classify = Some(classify);
        self
    }
}

impl<S> fmt::Debug for TowerTransport<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TowerTransport")
            .field("service", &core::any::type_name::<S>())
            .field("classify", &self.classify.is_some())
            .finish()
    }
}

impl<S, B> HttpTransport for TowerTransport<S>
where
    S: Service<Request<Full<Bytes>>, Response = Response<B>> + Clone + Send + Sync + 'static,
    S::Future: Send,
    S::Error: Into<BoxError>,
    B: Body<Data = Bytes> + Send + 'static,
    B::Error: Into<BoxError>,
{
    fn send(&self, req: Request<Bytes>, limit: usize) -> TransportFuture<'_> {
        Box::pin(send(self.service.clone(), self.classify, req, limit))
    }
}

async fn send<S, B>(
    mut service: S,
    classify: Option<Classifier>,
    req: Request<Bytes>,
    limit: usize,
) -> Result<Response<Bytes>, TransportError>
where
    S: Service<Request<Full<Bytes>>, Response = Response<B>>,
    S::Error: Into<BoxError>,
    B: Body<Data = Bytes>,
    B::Error: Into<BoxError>,
{
    let req = req.map(Full::new);
    poll_fn(|cx| service.poll_ready(cx))
        .await
        .map_err(|e| classified(classify, e.into(), TransportErrorKind::Other))?;
    let resp = service
        .call(req)
        .await
        .map_err(|e| classified(classify, e.into(), TransportErrorKind::Other))?;
    let (parts, body) = resp.into_parts();
    let too_large = TransportError::new(TransportErrorKind::TooLarge);
    if content_length(&parts.headers).is_some_and(|n| n > limit as u64)
        || body.size_hint().lower() > limit as u64
    {
        return Err(too_large);
    }
    let mut out = Vec::new();
    let mut body = pin!(body);
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|e| classified(classify, e.into(), TransportErrorKind::Body))?;
        // Trailers are not data.
        if let Ok(data) = frame.into_data() {
            if out.len() + data.len() > limit {
                return Err(too_large);
            }
            out.extend_from_slice(&data);
        }
    }
    Ok(Response::from_parts(parts, Bytes::from(out)))
}

fn classified(
    classify: Option<Classifier>,
    e: BoxError,
    default: TransportErrorKind,
) -> TransportError {
    let kind = classify.and_then(|f| f(&e)).unwrap_or(default);
    TransportError::with_source(kind, e)
}

/// The `Content-Length` header, if present and a number.
fn content_length(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(CONTENT_LENGTH)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use ::http::HeaderValue;

    use super::*;

    #[test]
    fn content_length_parses_or_ignores() {
        let mut h = HeaderMap::new();
        assert_eq!(content_length(&h), None);
        h.insert(CONTENT_LENGTH, HeaderValue::from_static("42"));
        assert_eq!(content_length(&h), Some(42));
        h.insert(CONTENT_LENGTH, HeaderValue::from_static(" 7 "));
        assert_eq!(content_length(&h), Some(7));
        h.insert(CONTENT_LENGTH, HeaderValue::from_static("many"));
        assert_eq!(content_length(&h), None);
    }
}
