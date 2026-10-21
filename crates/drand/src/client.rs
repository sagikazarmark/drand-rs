//! An async client that fetches and verifies beacons from several relays.
//!
//! Runs on native targets (on tokio's timer, which needs a tokio runtime) and on
//! `wasm32-unknown-unknown` (the JS host's `fetch` and `setTimeout`, so in
//! browsers and Cloudflare Workers, with no tokio). Not on WASI: use
//! [`crate::http`] with the host's HTTP API there.
//!
//! HTTP goes through [`HttpTransport`]. With the `reqwest` feature the default
//! is `ReqwestTransport`, which needs a TLS feature: `default-tls` (on by
//! default, reqwest's default backend), `rustls` or `native-tls`. Otherwise
//! [`ClientBuilder::http_transport`] is mandatory: a `ReqwestTransport` over
//! your own `reqwest::Client`, a `TowerTransport` (feature `tower`) over any
//! `tower::Service` (`tower-reqwest`, or `tower-worker-fetch` on Cloudflare
//! Workers), or any other `HttpTransport` (then `reqwest` stays out of the
//! build).
//!
//! The client, [`HttpTransport`] and their futures are `Send` on every target,
//! `wasm32` included, so they can live in shared state and run inside
//! `Send`-requiring frameworks (tower, axum, Restate) without wrapping. JS
//! futures (`fetch`, `setTimeout`) are not `Send`, but `wasm32-unknown-unknown`
//! is single-threaded: the client and `ReqwestTransport` wrap them in
//! `send_wrapper::SendWrapper`, which panics rather than misbehaves if a
//! future is ever polled from another thread. A transport over a JS API does
//! the same with [`assume_send`]. Threaded wasm (the `atomics` target feature)
//! is rejected at compile time.
//!
//! ```no_run
//! # async fn demo() -> Result<(), drand::client::Error> {
//! use std::time::Duration;
//! use drand::{chains, client::{Client, Fetch}};
//!
//! let client = Client::builder(chains::quicknet()).build()?;
//! let latest = client.latest().await?;
//! let next = client.wait_for(latest.round() + 1, Duration::from_secs(30)).await?;
//!
//! // In a durable executor's retried step: one attempt, no sleeping.
//! match client.fetch(next.round() + 1).await? {
//!     Fetch::Ready(beacon) => { /* use beacon.randomness() */ }
//!     Fetch::NotYet { due_at } => { /* retry after due_at */ }
//!     _ => {}
//! }
//! # Ok(()) }
//! ```

mod transport;

use std::{
    fmt,
    num::NonZeroU64,
    string::{String, ToString},
    sync::Arc,
    time::Duration,
    vec::Vec,
};

use core::{future::Future, pin::pin};

use ::http::{HeaderValue, Method, Request, header::USER_AGENT};
use bytes::Bytes;
use futures_util::{
    StreamExt,
    future::{Either, select},
    stream::FuturesUnordered,
};
use web_time::{SystemTime, UNIX_EPOCH};

#[cfg(feature = "test-chain")]
use crate::TestChain;
use crate::{
    ChainHash, ChainInfo, Schedule, UnixTime, VerifiedBeacon, Verifier,
    http::{self, Answer, GRACE, MAX_BODY_LEN, Relay, RoundRef},
};
#[cfg(feature = "reqwest")]
pub use transport::ReqwestTransport;
#[cfg(feature = "tower")]
pub use transport::{BoxError, TowerTransport};
pub use transport::{
    HttpTransport, TransportError, TransportErrorKind, TransportFuture, assume_send,
};

/// The result of one fetch attempt.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fetch {
    /// A verified beacon.
    Ready(VerifiedBeacon),
    /// The round is not published yet (or not yet past its grace period).
    NotYet {
        /// When the round is due.
        due_at: UnixTime,
    },
}

/// How relays are asked.
///
/// Relays are asked in order; the next one starts after `hedge_delay`, or as
/// soon as the previous one fails. The first verified beacon wins. A zero delay
/// asks all relays at once.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Strategy {
    /// Head start each relay gets before the next one is asked.
    pub hedge_delay: Duration,
}

impl Strategy {
    /// Hedged requests with the given head start.
    #[must_use]
    pub fn hedged(hedge_delay: Duration) -> Self {
        Self { hedge_delay }
    }
}

impl Default for Strategy {
    /// 500 ms head start: usually one request per fetch.
    fn default() -> Self {
        Self::hedged(Duration::from_millis(500))
    }
}

/// The wall clock.
///
/// A test clock must advance together with tokio's (paused) time:
/// [`Client::wait_for`] sleeps on tokio (on native targets) and computes due
/// times from the clock.
pub trait Clock: Send + Sync + 'static {
    /// The current time.
    fn now(&self) -> UnixTime;
}

/// The system clock (`Date.now()` on wasm).
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> UnixTime {
        // `web_time::SystemTime` is not `std`'s on wasm.
        UnixTime::from_millis(match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(d) => i64::try_from(d.as_millis()).unwrap_or(i64::MAX),
            Err(e) => i64::try_from(e.duration().as_millis()).map_or(i64::MIN, |ms| -ms),
        })
    }
}

/// Client errors.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The chain cannot be verified (unsupported scheme, invalid public key).
    #[error(transparent)]
    Chain(crate::Error),
    /// Round 0 was requested; rounds start at 1.
    #[error("round 0 does not exist")]
    RoundZero,
    /// Invalid client configuration (no relays, no transport and no default,
    /// mismatched test chain).
    #[error("client configuration: {0}")]
    Config(&'static str),
    /// The default transport (`ReqwestTransport`) could not be built.
    #[error("building the default HTTP transport")]
    DefaultTransport(#[source] TransportError),
    /// The round is past due (plus grace) and no relay produced a valid beacon.
    #[error("no relay produced a beacon: {}", list(.0))]
    Relays(Vec<RelayError>),
    /// [`Client::latest`] before the chain has started.
    #[error("the chain has not started")]
    NotStarted,
    /// [`Client::wait_for`] gave up. `last` holds the last attempt's failures,
    /// telling "drand stalled" (`NotYet`) from "relays unreachable".
    #[error("round {round} unavailable before the timeout: {}", list(.last))]
    Timeout {
        /// The round waited for.
        round: u64,
        /// Failures of the last attempt.
        last: Vec<RelayError>,
    },
}

impl Error {
    /// Whether trying again later can succeed.
    ///
    /// Every relay-level failure is retryable: a round that is due always
    /// comes into existence eventually, and treating a stall as terminal is
    /// worse than one extra retry. Configuration and chain errors are not.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::Relays(_) | Self::NotStarted | Self::Timeout { .. }
        )
    }
}

fn list(errors: &[RelayError]) -> String {
    if errors.is_empty() {
        return "no relay answered".into();
    }
    errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

/// One relay's failure.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
#[error("{relay}: {kind}")]
pub struct RelayError {
    /// The relay.
    pub relay: Relay,
    /// What went wrong.
    pub kind: RelayErrorKind,
}

/// What went wrong with one relay.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum RelayErrorKind {
    /// A "not yet" answer (425, or 404 within its grace window), recorded once
    /// the round was past due plus grace.
    #[error("round not published yet")]
    NotYet,
    /// 404 well after the round was due.
    #[error("round not found")]
    NotFound,
    /// Any other unsuccessful status.
    #[error("HTTP {0}")]
    Status(u16),
    /// The request timed out.
    #[error("timed out")]
    Timeout,
    /// The response exceeded [`MAX_BODY_LEN`].
    #[error("response too large")]
    BodyTooLarge,
    /// The relay served something that is not a valid beacon for the round.
    #[error("invalid beacon: {0}")]
    Invalid(crate::Error),
    /// A transport failure other than a timeout or an oversized body (those
    /// are [`RelayErrorKind::Timeout`] and [`RelayErrorKind::BodyTooLarge`]).
    #[error(transparent)]
    Transport(TransportError),
}

impl From<TransportError> for RelayErrorKind {
    fn from(e: TransportError) -> Self {
        match e.kind() {
            TransportErrorKind::Timeout => Self::Timeout,
            TransportErrorKind::TooLarge => Self::BodyTooLarge,
            _ => Self::Transport(e),
        }
    }
}

type SharedTransport = Arc<dyn HttpTransport>;

enum Transport {
    Http {
        transport: SharedTransport,
        relays: Vec<Relay>,
    },
    #[cfg(feature = "test-chain")]
    Test(TestChain),
}

struct Inner {
    chain: ChainInfo,
    hash: ChainHash,
    schedule: Schedule,
    verifier: Verifier,
    transport: Transport,
    strategy: Strategy,
    timeout: Duration,
    user_agent: HeaderValue,
    clock: Arc<dyn Clock>,
}

/// Fetches and verifies beacons of one chain. Cheap to clone.
#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut d = f.debug_struct("Client");
        d.field("chain", &self.inner.hash);
        match &self.inner.transport {
            Transport::Http { relays, .. } => d.field("relays", relays),
            #[cfg(feature = "test-chain")]
            Transport::Test(_) => d.field("relays", &"test chain"),
        };
        d.finish_non_exhaustive()
    }
}

/// Builds a [`Client`].
pub struct ClientBuilder {
    chain: ChainInfo,
    relays: Option<Vec<Relay>>,
    strategy: Strategy,
    request_timeout: Duration,
    user_agent: String,
    transport: Option<SharedTransport>,
    clock: Arc<dyn Clock>,
    #[cfg(feature = "test-chain")]
    test: Option<TestChain>,
}

impl fmt::Debug for ClientBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientBuilder")
            .field("chain", &self.chain.hash())
            .field("relays", &self.relays)
            .field("strategy", &self.strategy)
            .field("request_timeout", &self.request_timeout)
            .field("user_agent", &self.user_agent)
            .finish_non_exhaustive()
    }
}

impl ClientBuilder {
    /// Relays to ask, in order. Default: [`http::relays::defaults`] for the chain.
    #[must_use]
    pub fn relays(mut self, relays: Vec<Relay>) -> Self {
        self.relays = Some(relays);
        self
    }

    /// How relays are asked. Default: [`Strategy::default`].
    #[must_use]
    pub fn strategy(mut self, strategy: Strategy) -> Self {
        self.strategy = strategy;
        self
    }

    /// Per-request timeout (default 5 s). Enforced by the client around each
    /// [`HttpTransport::send`], so it holds for any transport.
    ///
    /// Asked for the next round, a relay may hold the request until that round
    /// is published: up to a full period (3 s on `quicknet`, 30 s on
    /// `default`). A shorter timeout is still correct (the attempt reports the
    /// round as not yet published), it just takes another request.
    #[must_use]
    pub fn request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    /// The `User-Agent` sent with each request.
    #[must_use]
    pub fn user_agent(mut self, user_agent: impl Into<String>) -> Self {
        self.user_agent = user_agent.into();
        self
    }

    /// Sends requests through `transport` instead of the default
    /// `ReqwestTransport`: e.g. `ReqwestTransport::new(client)` over a
    /// `reqwest::Client` with a proxy, `TowerTransport::new(service)` over a
    /// `tower::Service`, or your own [`HttpTransport`]. Mandatory without the
    /// `reqwest` feature.
    #[must_use]
    pub fn http_transport(mut self, transport: impl HttpTransport) -> Self {
        self.transport = Some(Arc::new(transport));
        self
    }

    /// The wall clock (default: [`SystemClock`]).
    #[must_use]
    pub fn clock(mut self, clock: impl Clock) -> Self {
        self.clock = Arc::new(clock);
        self
    }

    /// Serves beacons from a [`TestChain`] in process instead of asking relays:
    /// "not yet" before a round is due, signed beacons after. The chain passed
    /// to [`Client::builder`] must be `chain.chain()`. `build()` then skips the
    /// relay and TLS checks.
    #[cfg(feature = "test-chain")]
    #[must_use]
    pub fn test_transport(mut self, chain: TestChain) -> Self {
        self.test = Some(chain);
        self
    }

    /// Builds the client.
    ///
    /// # Errors
    ///
    /// [`Error::Chain`] for unverifiable chains, [`Error::Config`] with no
    /// relays, an invalid relay URL or user agent, or no
    /// [`ClientBuilder::http_transport`] and no default (the `reqwest` feature
    /// with a TLS feature), and [`Error::DefaultTransport`] if the default
    /// transport cannot be built.
    pub fn build(self) -> Result<Client, Error> {
        let verifier = Verifier::new(&self.chain).map_err(Error::Chain)?;
        let hash = self.chain.hash();

        #[cfg(feature = "test-chain")]
        let test = self.test;
        #[cfg(not(feature = "test-chain"))]
        let test: Option<core::convert::Infallible> = None;

        let transport = match test {
            #[cfg(feature = "test-chain")]
            Some(chain) => {
                if *chain.chain() != self.chain {
                    return Err(Error::Config("the test chain does not match the chain"));
                }
                Transport::Test(chain)
            }
            #[cfg(not(feature = "test-chain"))]
            Some(never) => match never {},
            None => {
                let relays = self.relays.unwrap_or_else(|| http::relays::defaults(&hash));
                if relays.is_empty() {
                    return Err(Error::Config("no relays"));
                }
                // Configuration errors must fail here, not as retryable relay errors.
                for relay in &relays {
                    if !valid_url(&relay.round_url(&hash, RoundRef::Latest)) {
                        return Err(Error::Config("invalid relay URL"));
                    }
                }
                let transport = match self.transport {
                    Some(transport) => transport,
                    None => default_transport()?,
                };
                Transport::Http { transport, relays }
            }
        };

        let user_agent = HeaderValue::from_str(&self.user_agent)
            .map_err(|_| Error::Config("invalid user agent"))?;
        Ok(Client {
            inner: Arc::new(Inner {
                schedule: self.chain.schedule(),
                chain: self.chain,
                hash,
                verifier,
                transport,
                strategy: self.strategy,
                timeout: self.request_timeout,
                user_agent,
                clock: self.clock,
            }),
        })
    }
}

/// An absolute `http(s)` URL with a host, as every parser that sees it later
/// accepts it: `Uri` in the request (which alone accepts `http://[zz]`) and,
/// in reqwest, `Url`. The same URLs are valid whatever the features.
fn valid_url(url: &str) -> bool {
    let Ok(uri) = url.parse::<::http::Uri>() else {
        return false;
    };
    let scheme_ok = matches!(uri.scheme_str(), Some("http" | "https"));
    let host_ok = match uri.host() {
        None | Some("") => false,
        Some(host) if host.starts_with('[') => host
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .is_some_and(|h| h.parse::<core::net::Ipv6Addr>().is_ok()),
        Some(_) => true,
    };
    let ok = scheme_ok && host_ok;
    #[cfg(feature = "reqwest")]
    let ok = ok && reqwest::Url::parse(url).is_ok_and(|u| u.has_host());
    ok
}

#[cfg(all(
    feature = "reqwest",
    any(feature = "default-tls", feature = "rustls", feature = "native-tls")
))]
fn default_transport() -> Result<SharedTransport, Error> {
    ReqwestTransport::try_default()
        .map(|t| Arc::new(t) as SharedTransport)
        .map_err(Error::DefaultTransport)
}

#[cfg(all(
    feature = "reqwest",
    not(any(feature = "default-tls", feature = "rustls", feature = "native-tls"))
))]
fn default_transport() -> Result<SharedTransport, Error> {
    Err(Error::Config(
        "no TLS backend: enable the `default-tls`, `rustls` or `native-tls` feature, or pass a transport",
    ))
}

#[cfg(not(feature = "reqwest"))]
fn default_transport() -> Result<SharedTransport, Error> {
    Err(Error::Config(
        "no default transport: enable the `reqwest` feature, or pass a transport",
    ))
}

/// The outcome of asking every relay once.
struct Attempt {
    beacon: Option<VerifiedBeacon>,
    errors: Vec<RelayError>,
}

impl Client {
    /// A builder for `chain`, which must come from [`crate::chains`],
    /// [`ChainInfo::from_json`] or a trusted [`ChainInfo::new`].
    #[must_use]
    pub fn builder(chain: ChainInfo) -> ClientBuilder {
        ClientBuilder {
            chain,
            relays: None,
            strategy: Strategy::default(),
            request_timeout: Duration::from_secs(5),
            user_agent: concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION")).into(),
            transport: None,
            clock: Arc::new(SystemClock),
            #[cfg(feature = "test-chain")]
            test: None,
        }
    }

    /// The chain.
    #[must_use]
    pub fn chain(&self) -> &ChainInfo {
        &self.inner.chain
    }

    /// The chain's verifier (e.g. to verify a persisted [`crate::Beacon`] again).
    #[must_use]
    pub fn verifier(&self) -> &Verifier {
        &self.inner.verifier
    }

    fn now(&self) -> UnixTime {
        self.inner.clock.now()
    }

    /// One attempt, no sleeping: what a durable executor's retried step calls.
    ///
    /// [`Fetch::Ready`] if any relay served a verified beacon. Otherwise
    /// [`Fetch::NotYet`] while the round is not past due plus
    /// [`GRACE`] (judged when the attempt finishes), and
    /// [`Error::Relays`] after that. [`Error::RoundZero`] for round 0.
    pub fn fetch(&self, round: u64) -> impl Future<Output = Result<Fetch, Error>> + Send {
        assume_send(self.fetch_inner(round))
    }

    async fn fetch_inner(&self, round: u64) -> Result<Fetch, Error> {
        let round = NonZeroU64::new(round).ok_or(Error::RoundZero)?;
        let attempt = self.attempt(round).await;
        if let Some(beacon) = attempt.beacon {
            return Ok(Fetch::Ready(beacon));
        }
        let due = self
            .inner
            .schedule
            .round_time(round.get())
            .unwrap_or(UnixTime::from_millis(i64::MAX));
        if self.now() < due.saturating_add(GRACE) {
            Ok(Fetch::NotYet { due_at: due })
        } else {
            Err(Error::Relays(attempt.errors))
        }
    }

    /// The latest round by the clock (not the relays' cached `latest`).
    ///
    /// If the relays say that round is not published yet, falls back to the one
    /// before, so it never returns "not yet". Before the chain has started (or
    /// before round 1 is available): [`Error::NotStarted`].
    pub fn latest(&self) -> impl Future<Output = Result<VerifiedBeacon, Error>> + Send {
        assume_send(self.latest_inner())
    }

    async fn latest_inner(&self) -> Result<VerifiedBeacon, Error> {
        let Some(round) = self.inner.schedule.round_at(self.now()) else {
            return Err(Error::NotStarted);
        };
        let attempt = self
            .attempt(NonZeroU64::new(round).expect("rounds start at 1"))
            .await;
        if let Some(b) = attempt.beacon {
            return Ok(b);
        }
        let not_yet = !attempt.errors.is_empty()
            && attempt
                .errors
                .iter()
                .all(|e| matches!(e.kind, RelayErrorKind::NotYet));
        if !not_yet {
            return Err(Error::Relays(attempt.errors));
        }
        match NonZeroU64::new(round - 1) {
            None => Err(Error::NotStarted),
            Some(previous) => {
                let attempt = self.attempt(previous).await;
                attempt.beacon.ok_or(Error::Relays(attempt.errors))
            }
        }
    }

    /// Waits until `round` is due, then fetches it, retrying with jittered
    /// backoff (250 ms doubling to 2 s) until it arrives or `timeout` passes.
    /// [`Error::RoundZero`] for round 0.
    pub fn wait_for(
        &self,
        round: u64,
        timeout: Duration,
    ) -> impl Future<Output = Result<VerifiedBeacon, Error>> + Send {
        assume_send(self.wait_for_inner(round, timeout))
    }

    async fn wait_for_inner(&self, round: u64, timeout: Duration) -> Result<VerifiedBeacon, Error> {
        let round = NonZeroU64::new(round).ok_or(Error::RoundZero)?;
        let deadline = deadline_after(timeout);
        if let Some(due) = self.inner.schedule.round_time(round.get()) {
            let wait = due.saturating_duration_since(self.now());
            if !wait.is_zero() {
                let wait = wait + Duration::from_millis(250);
                rt::sleep(wait.min(deadline.saturating_duration_since(rt::Instant::now()))).await;
            }
        }
        let mut delay = Duration::from_millis(250);
        let mut last = Vec::new();
        loop {
            // The deadline also bounds a slow attempt.
            let Some(attempt) = rt::timeout_at(deadline, self.attempt(round)).await else {
                return Err(Error::Timeout {
                    round: round.get(),
                    last,
                });
            };
            if let Some(b) = attempt.beacon {
                return Ok(b);
            }
            last = attempt.errors;
            let delay_now = jitter(delay);
            if rt::Instant::now() + delay_now > deadline {
                return Err(Error::Timeout {
                    round: round.get(),
                    last,
                });
            }
            rt::sleep(delay_now).await;
            delay = (delay * 2).min(Duration::from_secs(2));
        }
    }

    async fn attempt(&self, round: NonZeroU64) -> Attempt {
        match &self.inner.transport {
            Transport::Http { transport, relays } => {
                self.attempt_http(transport.as_ref(), relays, round).await
            }
            #[cfg(feature = "test-chain")]
            Transport::Test(chain) => {
                let due = self.inner.schedule.round_time(round.get());
                if due.is_none_or(|due| self.now() < due) {
                    return Attempt {
                        beacon: None,
                        errors: std::vec![RelayError {
                            relay: Relay::test_chain(),
                            kind: RelayErrorKind::NotYet,
                        }],
                    };
                }
                match self
                    .inner
                    .verifier
                    .verify_round(round.get(), &chain.sign(round.get()))
                {
                    Ok(b) => Attempt {
                        beacon: Some(b),
                        errors: Vec::new(),
                    },
                    Err(e) => Attempt {
                        beacon: None,
                        errors: std::vec![RelayError {
                            relay: Relay::test_chain(),
                            kind: RelayErrorKind::Invalid(e),
                        }],
                    },
                }
            }
        }
    }

    async fn attempt_http(
        &self,
        transport: &dyn HttpTransport,
        relays: &[Relay],
        round: NonZeroU64,
    ) -> Attempt {
        let ask = |i: usize| {
            let relay = &relays[i];
            async move { (i, self.ask(transport, relay, round).await) }
        };
        let mut errors: Vec<(usize, RelayErrorKind)> = Vec::new();
        let mut inflight = FuturesUnordered::new();
        let mut next = 0;
        loop {
            if inflight.is_empty() {
                if next == relays.len() {
                    break;
                }
                inflight.push(ask(next));
                next += 1;
            }
            // An answer wins over a hedge that is due at the same time.
            let answered = if next < relays.len() {
                let hedge = pin!(rt::sleep(self.inner.strategy.hedge_delay));
                match select(inflight.next(), hedge).await {
                    Either::Left((answered, _)) => answered,
                    Either::Right(_) => None,
                }
            } else {
                inflight.next().await
            };
            match answered {
                Some((_, Ok(beacon))) => {
                    return Attempt {
                        beacon: Some(beacon),
                        errors: collect(relays, errors),
                    };
                }
                Some((i, Err(kind))) => {
                    errors.push((i, kind));
                    if next < relays.len() {
                        inflight.push(ask(next));
                        next += 1;
                    }
                }
                // The hedge fired (the set is never empty here).
                None => {
                    inflight.push(ask(next));
                    next += 1;
                }
            }
        }
        Attempt {
            beacon: None,
            errors: collect(relays, errors),
        }
    }

    async fn ask(
        &self,
        transport: &dyn HttpTransport,
        relay: &Relay,
        round: NonZeroU64,
    ) -> Result<VerifiedBeacon, RelayErrorKind> {
        let req = RoundRef::Number(round);
        let url = relay.round_url(&self.inner.hash, req);
        let request = Request::builder()
            .method(Method::GET)
            .uri(url)
            .header(USER_AGENT, &self.inner.user_agent)
            .body(Bytes::new())
            // Unreachable: the URL and user agent are validated by `build()`.
            .map_err(|e| TransportError::with_source(TransportErrorKind::Other, e))?;
        let requested = self.now();
        // The timeout is the client's, so it holds for any transport.
        let deadline = deadline_after(self.inner.timeout);
        let resp = rt::timeout_at(deadline, transport.send(request, MAX_BODY_LEN))
            .await
            .ok_or(RelayErrorKind::Timeout)??;
        let status = resp.status().as_u16();
        let body = if resp.status().is_success() {
            resp.into_body()
        } else {
            Bytes::new()
        };
        let answer = http::classify(req, status, &body, &self.inner.schedule, requested);
        match answer {
            Ok(Answer::Beacon(beacon)) => self
                .inner
                .verifier
                .verify_round(round.get(), &beacon)
                .map_err(|e| {
                    tracing::warn!(%relay, round = round.get(), error = %e, "relay served an invalid beacon");
                    RelayErrorKind::Invalid(e)
                }),
            Ok(Answer::NotYet) => Err(RelayErrorKind::NotYet),
            Ok(Answer::NotFound) => Err(RelayErrorKind::NotFound),
            Ok(_) => Err(RelayErrorKind::Status(status)),
            Err(e) => {
                tracing::warn!(%relay, round = round.get(), error = %e, "relay served an invalid response");
                Err(RelayErrorKind::Invalid(e))
            }
        }
    }
}

fn collect(relays: &[Relay], mut errors: Vec<(usize, RelayErrorKind)>) -> Vec<RelayError> {
    errors.sort_by_key(|(i, _)| *i);
    errors
        .into_iter()
        .map(|(i, kind)| RelayError {
            relay: relays[i].clone(),
            kind,
        })
        .collect()
}

/// `timeout` from now; a huge timeout means "no timeout" (and must not overflow).
fn deadline_after(timeout: Duration) -> rt::Instant {
    let now = rt::Instant::now();
    now.checked_add(timeout)
        .unwrap_or_else(|| now + Duration::from_hours(100 * 365 * 24))
}

/// What the client needs from the async runtime.
#[cfg(not(target_arch = "wasm32"))]
mod rt {
    use core::future::Future;

    pub(super) use tokio::time::{Instant, sleep};

    /// `f`, at most until `deadline`.
    pub(super) async fn timeout_at<F: Future>(deadline: Instant, f: F) -> Option<F::Output> {
        tokio::time::timeout_at(deadline, f).await.ok()
    }
}

/// What the client needs from the async runtime: the JS host's.
#[cfg(target_arch = "wasm32")]
mod rt {
    use core::{future::Future, pin::pin, time::Duration};

    use futures_util::future::{Either, select};
    pub(super) use web_time::Instant;

    /// `setTimeout` takes at most 2^31 - 1 ms (and fires at once beyond that).
    const MAX_TIMEOUT: Duration = Duration::from_millis(i32::MAX as u64);

    pub(super) async fn sleep(mut d: Duration) {
        while !d.is_zero() {
            let step = d.min(MAX_TIMEOUT);
            gloo_timers::future::sleep(step).await;
            d -= step;
        }
    }

    /// `f`, at most until `deadline`.
    pub(super) async fn timeout_at<F: Future>(deadline: Instant, f: F) -> Option<F::Output> {
        let timer = pin!(sleep(deadline.saturating_duration_since(Instant::now())));
        match select(pin!(f), timer).await {
            Either::Left((output, _)) => Some(output),
            Either::Right(_) => None,
        }
    }
}

/// ±20 % jitter, without a random number generator: clock microseconds mixed
/// with a counter (some platforms' clocks are coarse).
fn jitter(d: Duration) -> Duration {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |t| t.subsec_micros());
    let mixed = micros.wrapping_add(COUNTER.fetch_add(7919, Ordering::Relaxed)) % 1000;
    let factor = 0.8 + 0.4 * f64::from(mixed) / 1000.0;
    d.mul_f64(factor)
}

#[allow(dead_code)]
fn assert_send(client: &Client) {
    fn send<T: Send>(_: T) {}
    fn send_sync<T: Send + Sync>(_: T) {}
    send(client.fetch(1));
    send(client.latest());
    send(client.wait_for(1, Duration::ZERO));
    send_sync(client.clone());
}
