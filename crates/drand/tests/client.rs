//! Client behavior against in-process mock relays.

#![cfg(all(
    feature = "client",
    feature = "test-chain",
    not(target_arch = "wasm32")
))]

use std::{
    net::SocketAddr,
    num::NonZeroU32,
    sync::{
        Arc, Mutex,
        atomic::{AtomicI64, Ordering},
    },
    time::Duration,
};

use axum::{
    Router,
    extract::Path,
    http::{HeaderMap, StatusCode},
    routing::get,
};
use drand::{
    Beacon, ChainInfo, Error as CoreError, Scheme, TestChain, UnixTime, chains,
    client::{Client, Clock, Error, Fetch, RelayErrorKind, Strategy},
    http::{GRACE, MAX_BODY_LEN, Relay},
};

const GENESIS: i64 = 1_700_000_000;

fn test_chain() -> TestChain {
    TestChain::new(
        [3; 32],
        Scheme::BlsUnchainedG1Rfc9380,
        NonZeroU32::new(3).unwrap(),
        GENESIS,
        "mock",
    )
    .unwrap()
}

/// A settable clock.
#[derive(Clone, Default)]
struct FixedClock(Arc<AtomicI64>);

impl FixedClock {
    fn at(ms: i64) -> Self {
        Self(Arc::new(AtomicI64::new(ms)))
    }
    fn set(&self, ms: i64) {
        self.0.store(ms, Ordering::SeqCst);
    }
}

impl Clock for FixedClock {
    fn now(&self) -> UnixTime {
        UnixTime::from_millis(self.0.load(Ordering::SeqCst))
    }
}

fn grace_ms() -> i64 {
    i64::try_from(GRACE.as_millis()).unwrap()
}

fn due_ms(round: u64) -> i64 {
    test_chain()
        .chain()
        .schedule()
        .round_time(round)
        .unwrap()
        .as_millis()
}

struct Reply {
    status: u16,
    body: String,
    delay: Duration,
}

impl Reply {
    fn ok(beacon: &Beacon) -> Self {
        Self::status(200, serde_json::to_string(beacon).unwrap())
    }
    fn status(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            body: body.into(),
            delay: Duration::ZERO,
        }
    }
    fn delayed(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }
}

type Handler = Arc<dyn Fn(u64) -> Reply + Send + Sync>;

/// Starts a mock relay serving both API versions; returns its base URL.
async fn relay(
    handler: impl Fn(u64) -> Reply + Send + Sync + 'static,
    agents: Option<Arc<Mutex<Vec<String>>>>,
) -> String {
    let handler: Handler = Arc::new(handler);
    let serve = move |round: String, headers: HeaderMap| {
        let handler = handler.clone();
        let agents = agents.clone();
        async move {
            if let Some(agents) = agents {
                let ua = headers
                    .get("user-agent")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default();
                agents.lock().unwrap().push(ua.to_owned());
            }
            let reply = handler(round.parse().unwrap_or(0));
            tokio::time::sleep(reply.delay).await;
            (StatusCode::from_u16(reply.status).unwrap(), reply.body)
        }
    };
    let s1 = serve.clone();
    let app = Router::new()
        .route(
            "/{chain}/public/{round}",
            get(move |Path((_, round)): Path<(String, String)>, h: HeaderMap| s1(round, h)),
        )
        .route(
            "/v2/chains/{chain}/rounds/{round}",
            get(move |Path((_, round)): Path<(String, String)>, h: HeaderMap| serve(round, h)),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

fn client(relays: Vec<Relay>, clock: FixedClock) -> Client {
    Client::builder(test_chain().chain().clone())
        .relays(relays)
        .http_client(reqwest::Client::new())
        .clock(clock)
        .strategy(Strategy::hedged(Duration::from_millis(50)))
        .request_timeout(Duration::from_secs(2))
        .build()
        .unwrap()
}

fn signed(round: u64) -> Beacon {
    test_chain().sign(round)
}

#[tokio::test]
async fn first_valid_beacon_wins_across_api_versions() {
    let agents = Arc::new(Mutex::new(Vec::new()));
    let a = relay(|r| Reply::ok(&signed(r)), Some(agents.clone())).await;
    for relay in [Relay::v1(&a).unwrap(), Relay::v2(&a).unwrap()] {
        let c = client(vec![relay], FixedClock::at(due_ms(10) + 10));
        let Fetch::Ready(b) = c.fetch(10).await.unwrap() else {
            panic!()
        };
        assert_eq!(b.round(), 10);
        assert_eq!(b.chain_hash(), test_chain().chain().hash());
    }
    assert!(
        agents
            .lock()
            .unwrap()
            .iter()
            .all(|ua| ua.starts_with("drand/"))
    );
}

#[tokio::test]
async fn not_yet_before_due_and_relays_error_after_grace() {
    let a = relay(|_| Reply::status(425, "Requested future beacon"), None).await;
    let b = relay(|_| Reply::status(404, ""), None).await;
    let clock = FixedClock::at(due_ms(10) - 5_000);
    let c = client(
        vec![Relay::v2(&a).unwrap(), Relay::v1(&b).unwrap()],
        clock.clone(),
    );
    assert_eq!(
        c.fetch(10).await.unwrap(),
        Fetch::NotYet {
            due_at: UnixTime::from_millis(due_ms(10))
        }
    );
    // Inside the grace window it is still "not yet".
    clock.set(due_ms(10) + grace_ms() - 1);
    assert!(matches!(c.fetch(10).await.unwrap(), Fetch::NotYet { .. }));
    // Past due + grace: an error, retryable, telling stall (425) from not found (404).
    clock.set(due_ms(10) + 60_000);
    let err = c.fetch(10).await.unwrap_err();
    assert!(err.is_retryable());
    let Error::Relays(errors) = err else {
        panic!("{err}")
    };
    assert!(matches!(errors[0].kind, RelayErrorKind::NotYet));
    assert!(matches!(errors[1].kind, RelayErrorKind::NotFound));
}

#[tokio::test]
async fn invalid_or_wrong_round_beacons_fall_through_to_the_next_relay() {
    let tampered = relay(
        |r| {
            let mut b = signed(r);
            b.signature[3] ^= 1;
            Reply::ok(&b)
        },
        None,
    )
    .await;
    let wrong_round = relay(|r| Reply::ok(&signed(r + 1)), None).await;
    let other_chain = relay(|r| Reply::ok(&chains_beacon(r)), None).await;
    let good = relay(|r| Reply::ok(&signed(r)), None).await;
    let c = client(
        [&tampered, &wrong_round, &other_chain, &good]
            .iter()
            .map(|u| Relay::v1(u).unwrap())
            .collect(),
        FixedClock::at(due_ms(10)),
    );
    let Fetch::Ready(b) = c.fetch(10).await.unwrap() else {
        panic!()
    };
    assert_eq!(b.round(), 10);
}

/// A beacon signed by a different test chain.
fn chains_beacon(round: u64) -> Beacon {
    TestChain::new(
        [4; 32],
        Scheme::BlsUnchainedG1Rfc9380,
        NonZeroU32::new(3).unwrap(),
        GENESIS,
        "mock",
    )
    .unwrap()
    .sign(round)
}

#[tokio::test]
async fn every_relay_failing_is_reported_per_relay() {
    let e500 = relay(|_| Reply::status(500, "boom"), None).await;
    let e429 = relay(|_| Reply::status(429, ""), None).await;
    let e403 = relay(|_| Reply::status(403, ""), None).await;
    let bad = relay(
        |r| Reply::status(200, format!(r#"{{"round":{r},"signature":"zz"}}"#)),
        None,
    )
    .await;
    let big = relay(|_| Reply::status(200, " ".repeat(MAX_BODY_LEN + 1)), None).await;
    let c = client(
        [&e500, &e429, &e403, &bad, &big]
            .iter()
            .map(|u| Relay::v1(u).unwrap())
            .collect(),
        FixedClock::at(due_ms(10) + 60_000),
    );
    let Error::Relays(errors) = c.fetch(10).await.unwrap_err() else {
        panic!()
    };
    assert_eq!(errors.len(), 5);
    assert!(matches!(errors[0].kind, RelayErrorKind::Status(500)));
    assert!(matches!(errors[1].kind, RelayErrorKind::Status(429)));
    assert!(matches!(errors[2].kind, RelayErrorKind::Status(403)));
    assert!(matches!(errors[3].kind, RelayErrorKind::Invalid(_)));
    assert!(matches!(errors[4].kind, RelayErrorKind::BodyTooLarge));
}

#[tokio::test]
async fn slow_relays_are_hedged() {
    let slow = relay(
        |r| Reply::ok(&signed(r)).delayed(Duration::from_secs(3)),
        None,
    )
    .await;
    let fast = relay(|r| Reply::ok(&signed(r)), None).await;
    let c = Client::builder(test_chain().chain().clone())
        .relays(vec![Relay::v1(&slow).unwrap(), Relay::v1(&fast).unwrap()])
        .http_client(reqwest::Client::new())
        .clock(FixedClock::at(due_ms(10)))
        .strategy(Strategy::hedged(Duration::from_millis(100)))
        .build()
        .unwrap();
    let started = std::time::Instant::now();
    assert!(matches!(c.fetch(10).await.unwrap(), Fetch::Ready(_)));
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn request_timeout_applies_to_passed_clients() {
    let slow = relay(
        |r| Reply::ok(&signed(r)).delayed(Duration::from_secs(5)),
        None,
    )
    .await;
    let c = Client::builder(test_chain().chain().clone())
        .relays(vec![Relay::v1(&slow).unwrap()])
        .http_client(reqwest::Client::new())
        .clock(FixedClock::at(due_ms(10) + 60_000))
        .request_timeout(Duration::from_millis(200))
        .build()
        .unwrap();
    let Error::Relays(errors) = c.fetch(10).await.unwrap_err() else {
        panic!()
    };
    assert!(matches!(errors[0].kind, RelayErrorKind::Timeout));
}

#[tokio::test]
async fn latest_falls_back_to_the_previous_round() {
    let now = due_ms(100) + 100;
    let a = relay(
        |r| {
            if r >= 100 {
                Reply::status(425, "")
            } else {
                Reply::ok(&signed(r))
            }
        },
        None,
    )
    .await;
    let c = client(vec![Relay::v2(&a).unwrap()], FixedClock::at(now));
    assert_eq!(c.latest().await.unwrap().round(), 99);

    let before = client(
        vec![Relay::v2(&a).unwrap()],
        FixedClock::at(GENESIS * 1000 - 1),
    );
    assert!(matches!(before.latest().await, Err(Error::NotStarted)));
}

#[tokio::test]
async fn round_zero_and_configuration_errors_are_terminal() {
    let c = client(
        vec![Relay::v1("http://127.0.0.1:9").unwrap()],
        FixedClock::at(0),
    );
    let err = c.fetch(0).await.unwrap_err();
    assert!(matches!(err, Error::RoundZero));
    assert!(!err.is_retryable());

    // A chain that is not built in has no default relays.
    let err = Client::builder(test_chain().chain().clone())
        .http_client(reqwest::Client::new())
        .build()
        .unwrap_err();
    assert!(matches!(err, Error::Config(_)));
    assert!(!err.is_retryable());

    // Unverifiable chains are rejected at build time.
    let err = Client::builder(chains::evmnet()).build().unwrap_err();
    assert!(matches!(err, Error::Chain(CoreError::UnsupportedScheme(_))));

    // The test transport must match the chain.
    let other: ChainInfo = chains::quicknet();
    let err = Client::builder(other)
        .test_transport(test_chain())
        .build()
        .unwrap_err();
    assert!(matches!(err, Error::Config(_)));
}

#[cfg(not(any(feature = "default-tls", feature = "rustls", feature = "native-tls")))]
#[test]
fn building_without_tls_or_a_client_fails() {
    let err = Client::builder(chains::quicknet()).build().unwrap_err();
    assert!(matches!(err, Error::Config(_)));
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
async fn bodies_without_content_length_are_capped_too() {
    let app = Router::new().route(
        "/{chain}/public/{round}",
        get(|| async {
            let chunks = (0..20).map(|_| Ok::<_, std::io::Error>(vec![b' '; 8 * 1024]));
            axum::body::Body::from_stream(futures_util::stream::iter(chunks))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let c = client(
        vec![Relay::v1(&format!("http://{addr}")).unwrap()],
        FixedClock::at(due_ms(10) + 60_000),
    );
    let Error::Relays(errors) = c.fetch(10).await.unwrap_err() else {
        panic!()
    };
    assert!(matches!(errors[0].kind, RelayErrorKind::BodyTooLarge));
}

#[tokio::test]
async fn configuration_errors_fail_at_build_time() {
    let base = || {
        Client::builder(test_chain().chain().clone())
            .http_client(reqwest::Client::new())
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
}

#[tokio::test]
async fn wait_for_honors_its_deadline_and_huge_timeouts() {
    // Relays that accept connections and never answer.
    let hang = relay(
        |r| Reply::ok(&signed(r)).delayed(Duration::from_secs(30)),
        None,
    )
    .await;
    let c = Client::builder(test_chain().chain().clone())
        .relays(vec![Relay::v1(&hang).unwrap(), Relay::v2(&hang).unwrap()])
        .http_client(reqwest::Client::new())
        .clock(FixedClock::at(due_ms(10) + 60_000))
        .build()
        .unwrap();
    let started = std::time::Instant::now();
    let err = c
        .wait_for(10, Duration::from_millis(500))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Timeout { .. }), "{err}");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );

    // Duration::MAX means "no timeout" and must not overflow.
    let tc = test_chain();
    let c = Client::builder(tc.chain().clone())
        .test_transport(tc)
        .clock(FixedClock::at(due_ms(10)))
        .build()
        .unwrap();
    assert_eq!(c.wait_for(10, Duration::MAX).await.unwrap().round(), 10);
}
