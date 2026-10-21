//! Client behavior with no network: a hand-written `HttpTransport`, the test
//! chain, and what `build()` does without a default transport.

#![cfg(all(
    feature = "client",
    feature = "test-chain",
    not(target_arch = "wasm32")
))]

mod common;

use std::{
    future::pending,
    sync::{Arc, Mutex},
    time::Duration,
};

use bytes::Bytes;
use common::{FixedClock, due_ms, signed, test_chain};
use drand::{
    UnixTime,
    client::{
        Client, Clock, Error, Fetch, HttpTransport, RelayErrorKind, TransportError,
        TransportErrorKind, TransportFuture,
    },
    http::Relay,
};
use http::{Method, Request, Response, header};

/// Without the `reqwest` feature (or without a TLS backend for it) there is no
/// default transport: `build()` needs `http_transport`.
#[cfg(not(all(
    feature = "reqwest",
    any(feature = "default-tls", feature = "rustls", feature = "native-tls")
)))]
#[test]
fn building_without_a_default_transport_fails() {
    let err = Client::builder(drand::chains::quicknet())
        .build()
        .unwrap_err();
    assert!(matches!(err, Error::Config(_)), "{err}");
    assert!(!err.is_retryable());
}

#[tokio::test]
async fn configuration_errors_fail_at_build_time() {
    let base = || {
        Client::builder(test_chain().chain().clone())
            .http_transport(Hang)
            .relays(vec![Relay::v1("http://127.0.0.1:9").unwrap()])
    };
    assert!(matches!(
        base().user_agent("bad\nagent").build(),
        Err(Error::Config(_))
    ));
    let odd = Relay::v1("http://[zz]").unwrap();
    assert!(matches!(
        base().relays(vec![odd]).build(),
        Err(Error::Config(_))
    ));
    assert!(matches!(
        base().relays(Vec::new()).build(),
        Err(Error::Config(_))
    ));
}

/// A clock that follows tokio's (paused) time.
#[derive(Clone)]
struct TokioClock {
    base_ms: i64,
    start: tokio::time::Instant,
}

impl Clock for TokioClock {
    fn now(&self) -> UnixTime {
        UnixTime::from_millis(
            self.base_ms + i64::try_from(self.start.elapsed().as_millis()).unwrap(),
        )
    }
}

#[tokio::test(start_paused = true)]
async fn wait_for_sleeps_until_due_with_the_test_transport() {
    let tc = test_chain();
    let clock = TokioClock {
        base_ms: due_ms(50) - 10_000,
        start: tokio::time::Instant::now(),
    };
    let c = Client::builder(tc.chain().clone())
        .test_transport(tc)
        .clock(clock.clone())
        .build()
        .unwrap();
    assert!(matches!(c.fetch(50).await.unwrap(), Fetch::NotYet { .. }));
    let b = c.wait_for(50, Duration::from_secs(30)).await.unwrap();
    assert_eq!(b.round(), 50);
    assert!(clock.now().as_millis() >= due_ms(50));
    // A round far in the future times out, reporting "not yet".
    let err = c
        .wait_for(1_000_000, Duration::from_secs(5))
        .await
        .unwrap_err();
    let Error::Timeout { last, .. } = &err else {
        panic!("{err}")
    };
    assert!(matches!(last[0].kind, RelayErrorKind::NotYet));
    assert!(err.is_retryable());
}

#[tokio::test]
async fn wait_for_accepts_huge_timeouts() {
    // Duration::MAX means "no timeout" and must not overflow.
    let tc = test_chain();
    let c = Client::builder(tc.chain().clone())
        .test_transport(tc)
        .clock(FixedClock::at(due_ms(10)))
        .build()
        .unwrap();
    assert_eq!(c.wait_for(10, Duration::MAX).await.unwrap().round(), 10);
}

type Answer = Box<dyn Fn(&str, u64) -> Result<(u16, String), TransportError> + Send + Sync>;

/// An in-memory transport: answers by relay host and round, no network.
struct Memory {
    answer: Answer,
    requests: Mutex<Vec<Request<Bytes>>>,
}

impl Memory {
    fn new(
        answer: impl Fn(&str, u64) -> Result<(u16, String), TransportError> + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(Self {
            answer: Box::new(answer),
            requests: Mutex::new(Vec::new()),
        })
    }
}

impl HttpTransport for Memory {
    fn send(&self, req: Request<Bytes>, limit: usize) -> TransportFuture<'_> {
        let host = req.uri().host().unwrap_or_default().to_owned();
        let round = req
            .uri()
            .path()
            .rsplit('/')
            .next()
            .and_then(|r| r.parse().ok())
            .unwrap_or(0);
        self.requests.lock().unwrap().push(req);
        Box::pin(async move {
            let (status, body) = (self.answer)(&host, round)?;
            if body.len() > limit {
                return Err(TransportError::new(TransportErrorKind::TooLarge));
            }
            Ok(Response::builder()
                .status(status)
                .body(Bytes::from(body))
                .unwrap())
        })
    }
}

#[tokio::test]
async fn an_in_memory_transport_drives_fetch_and_latest() {
    let memory = Memory::new(|_, r| Ok((200, serde_json::to_string(&signed(r)).unwrap())));
    let c = Client::builder(test_chain().chain().clone())
        .relays(vec![Relay::v1("http://memory").unwrap()])
        .http_transport(memory.clone())
        .clock(FixedClock::at(due_ms(100) + 100))
        .build()
        .unwrap();
    let Fetch::Ready(b) = c.fetch(10).await.unwrap() else {
        panic!()
    };
    assert_eq!(b.round(), 10);
    assert_eq!(c.latest().await.unwrap().round(), 100);

    let requests = memory.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    for req in requests.iter() {
        assert_eq!(req.method(), Method::GET);
        assert_eq!(req.uri().host(), Some("memory"));
        assert!(
            req.headers()[header::USER_AGENT]
                .to_str()
                .unwrap()
                .starts_with("drand/")
        );
    }
}

#[tokio::test]
async fn transport_errors_map_to_relay_errors() {
    let memory = Memory::new(|host, _| {
        Err(match host {
            "too-large" => TransportError::new(TransportErrorKind::TooLarge),
            "timeout" => TransportError::new(TransportErrorKind::Timeout),
            "connect" => TransportError::with_source(TransportErrorKind::Connect, "refused"),
            "body" => TransportError::new(TransportErrorKind::Body),
            _ => TransportError::new(TransportErrorKind::Other),
        })
    });
    let c = Client::builder(test_chain().chain().clone())
        .relays(
            ["too-large", "timeout", "connect", "body", "other"]
                .iter()
                .map(|h| Relay::v1(&format!("http://{h}")).unwrap())
                .collect(),
        )
        .http_transport(memory)
        .clock(FixedClock::at(due_ms(10) + 60_000))
        .build()
        .unwrap();
    let Error::Relays(errors) = c.fetch(10).await.unwrap_err() else {
        panic!()
    };
    assert_eq!(errors.len(), 5);
    assert!(matches!(errors[0].kind, RelayErrorKind::BodyTooLarge));
    assert!(matches!(errors[1].kind, RelayErrorKind::Timeout));
    // Other kinds keep the transport error, kind included.
    let RelayErrorKind::Transport(e) = &errors[2].kind else {
        panic!()
    };
    assert_eq!(e.kind(), TransportErrorKind::Connect);
    assert_eq!(e.to_string(), "connection failed: refused");
    assert!(
        errors[2]
            .to_string()
            .ends_with("connection failed: refused")
    );
    let RelayErrorKind::Transport(e) = &errors[3].kind else {
        panic!()
    };
    assert_eq!(e.kind(), TransportErrorKind::Body);
    let RelayErrorKind::Transport(e) = &errors[4].kind else {
        panic!()
    };
    assert_eq!(e.kind(), TransportErrorKind::Other);
    assert_eq!(e.to_string(), "transport error");
}

/// A transport whose requests never complete.
struct Hang;

impl HttpTransport for Hang {
    fn send(&self, _: Request<Bytes>, _: usize) -> TransportFuture<'_> {
        Box::pin(pending())
    }
}

#[tokio::test(start_paused = true)]
async fn hanging_transports_are_cut_off_by_the_request_timeout() {
    let c = Client::builder(test_chain().chain().clone())
        .relays(vec![Relay::v1("http://hang").unwrap()])
        .http_transport(Hang)
        .clock(FixedClock::at(due_ms(10) + 60_000))
        .request_timeout(Duration::from_millis(200))
        .build()
        .unwrap();
    let Error::Relays(errors) = c.fetch(10).await.unwrap_err() else {
        panic!()
    };
    assert!(matches!(errors[0].kind, RelayErrorKind::Timeout));
}
