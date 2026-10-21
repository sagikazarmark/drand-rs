//! `TowerTransport`, first over an in-process `tower::Service` with no network
//! (the client end to end, the body limit up front and while streaming, how
//! service and body errors are reported), then over a real HTTP stack,
//! `tower-reqwest` behind the `map_request` the README shows, against mock
//! relays.

#![cfg(all(
    feature = "client",
    feature = "tower",
    feature = "test-chain",
    not(target_arch = "wasm32")
))]

mod common;

use std::{
    collections::VecDeque,
    future::{Ready, ready},
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

use axum::{Router, extract::Path, response::IntoResponse, routing::get};
use bytes::Bytes;
use common::{FixedClock, due_ms, signed, test_chain};
use drand::{
    client::{
        BoxError, Client, Error, Fetch, HttpTransport, RelayErrorKind, TowerTransport,
        TransportErrorKind,
    },
    http::{MAX_BODY_LEN, Relay},
};
use http::{Method, Request, Response, StatusCode, header};
use http_body::{Body, Frame, SizeHint};
use http_body_util::Full;
use tower::ServiceBuilder;
use tower_reqwest::HttpClientService;
use tower_service::Service;

/// A streaming body: one frame per chunk, then `fail` if set. Counts the
/// frames it yields, so a test can see where reading stopped.
struct Chunks {
    frames: VecDeque<Bytes>,
    fail: Option<&'static str>,
    hint: Option<u64>,
    yielded: Arc<AtomicUsize>,
}

impl Chunks {
    fn new(chunks: impl IntoIterator<Item = Bytes>) -> Self {
        Self {
            frames: chunks.into_iter().collect(),
            fail: None,
            hint: None,
            yielded: Arc::default(),
        }
    }

    fn one(body: impl Into<Bytes>) -> Self {
        Self::new([body.into()])
    }

    fn failing(mut self, message: &'static str) -> Self {
        self.fail = Some(message);
        self
    }

    fn hinting(mut self, lower: u64) -> Self {
        self.hint = Some(lower);
        self
    }

    fn counting(mut self, yielded: &Arc<AtomicUsize>) -> Self {
        self.yielded = yielded.clone();
        self
    }
}

impl Body for Chunks {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        Poll::Ready(match self.frames.pop_front() {
            Some(chunk) => {
                self.yielded.fetch_add(1, Ordering::SeqCst);
                Some(Ok(Frame::data(chunk)))
            }
            None => self.fail.take().map(|m| Err(m.into())),
        })
    }

    fn is_end_stream(&self) -> bool {
        self.frames.is_empty() && self.fail.is_none()
    }

    fn size_hint(&self) -> SizeHint {
        let mut hint = SizeHint::new();
        if let Some(lower) = self.hint {
            hint.set_lower(lower);
        }
        hint
    }
}

#[derive(Debug, thiserror::Error)]
enum ServiceError {
    #[error("the service timed out")]
    Timeout,
    #[error("the service was refused")]
    Refused,
}

type Answer = Arc<dyn Fn(&str, u64) -> Result<Response<Chunks>, BoxError> + Send + Sync>;

/// A service answering by relay host and round, recording its requests.
#[derive(Clone)]
struct Mock {
    answer: Answer,
    requests: Arc<Mutex<Vec<Request<Full<Bytes>>>>>,
}

impl Mock {
    fn new(
        answer: impl Fn(&str, u64) -> Result<Response<Chunks>, BoxError> + Send + Sync + 'static,
    ) -> Self {
        Self {
            answer: Arc::new(answer),
            requests: Arc::default(),
        }
    }
}

impl Service<Request<Full<Bytes>>> for Mock {
    type Response = Response<Chunks>;
    type Error = BoxError;
    type Future = Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<Full<Bytes>>) -> Self::Future {
        let host = req.uri().host().unwrap_or_default().to_owned();
        let round = req
            .uri()
            .path()
            .rsplit('/')
            .next()
            .and_then(|r| r.parse().ok())
            .unwrap_or(0);
        let answer = (self.answer)(&host, round);
        self.requests.lock().unwrap().push(req);
        ready(answer)
    }
}

fn client(transport: TowerTransport<Mock>, relays: &[&str]) -> Client {
    Client::builder(test_chain().chain().clone())
        .relays(
            relays
                .iter()
                .map(|h| Relay::v1(&format!("http://{h}")).unwrap())
                .collect(),
        )
        .http_transport(transport)
        .clock(FixedClock::at(due_ms(100) + 100))
        .build()
        .unwrap()
}

fn beacon_json(round: u64) -> Vec<u8> {
    serde_json::to_vec(&signed(round)).unwrap()
}

#[tokio::test]
async fn a_tower_service_drives_fetch_and_latest() {
    // Bodies arrive in two frames, to be reassembled.
    let mock = Mock::new(|_, r| {
        let json = beacon_json(r);
        let (head, tail) = json.split_at(json.len() / 2);
        Ok(Response::new(Chunks::new([
            Bytes::copy_from_slice(head),
            Bytes::copy_from_slice(tail),
        ])))
    });
    let c = client(TowerTransport::new(mock.clone()), &["memory"]);
    let Fetch::Ready(b) = c.fetch(10).await.unwrap() else {
        panic!()
    };
    assert_eq!(b.round(), 10);
    assert_eq!(c.latest().await.unwrap().round(), 100);

    let requests = mock.requests.lock().unwrap();
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
        // A GET carries no body: the service can tell without reading it.
        assert!(req.body().is_end_stream());
    }
}

#[tokio::test]
async fn oversized_bodies_are_rejected_before_reading_when_announced() {
    let yielded = Arc::new(AtomicUsize::new(0));
    let by_header = yielded.clone();
    let by_hint = yielded.clone();
    let mock = Mock::new(move |host, r| {
        let body = Chunks::one(beacon_json(r));
        match host {
            "header" => Ok(Response::builder()
                .header(header::CONTENT_LENGTH, MAX_BODY_LEN + 1)
                .body(body.counting(&by_header))
                .unwrap()),
            _ => Ok(Response::new(
                body.hinting(MAX_BODY_LEN as u64 + 1).counting(&by_hint),
            )),
        }
    });
    let c = client(TowerTransport::new(mock), &["header", "hint"]);
    let Error::Relays(errors) = c.fetch(10).await.unwrap_err() else {
        panic!()
    };
    assert_eq!(errors.len(), 2);
    assert!(matches!(errors[0].kind, RelayErrorKind::BodyTooLarge));
    assert!(matches!(errors[1].kind, RelayErrorKind::BodyTooLarge));
    assert_eq!(yielded.load(Ordering::SeqCst), 0, "no frame was read");
}

#[tokio::test]
async fn oversized_bodies_stop_being_read_at_the_limit() {
    const CHUNK: usize = 1024;
    let total = MAX_BODY_LEN / CHUNK + 8;
    let yielded = Arc::new(AtomicUsize::new(0));
    let counter = yielded.clone();
    let mock = Mock::new(move |_, _| {
        Ok(Response::new(
            Chunks::new((0..total).map(|_| Bytes::from(vec![b'x'; CHUNK]))).counting(&counter),
        ))
    });
    let c = client(TowerTransport::new(mock), &["stream"]);
    let Error::Relays(errors) = c.fetch(10).await.unwrap_err() else {
        panic!()
    };
    assert!(matches!(errors[0].kind, RelayErrorKind::BodyTooLarge));
    // The frame that crosses the limit is the last one read.
    assert_eq!(yielded.load(Ordering::SeqCst), MAX_BODY_LEN / CHUNK + 1);
}

#[tokio::test]
async fn service_and_body_errors_are_reported_with_their_source() {
    let mock = Mock::new(|host, r| match host {
        "timeout" => Err(ServiceError::Timeout.into()),
        "refused" => Err(ServiceError::Refused.into()),
        _ => Ok(Response::new(
            Chunks::one(beacon_json(r)).failing("connection reset"),
        )),
    });
    let relays = ["timeout", "refused", "body"];

    // By default: `Other` from the service, `Body` while reading, source kept.
    let c = client(TowerTransport::new(mock.clone()), &relays);
    let Error::Relays(errors) = c.fetch(10).await.unwrap_err() else {
        panic!()
    };
    let kinds: Vec<_> = errors
        .iter()
        .map(|e| match &e.kind {
            RelayErrorKind::Transport(t) => (t.kind(), t.to_string()),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(
        kinds,
        [
            (
                TransportErrorKind::Other,
                "transport error: the service timed out".to_owned()
            ),
            (
                TransportErrorKind::Other,
                "transport error: the service was refused".to_owned()
            ),
            (
                TransportErrorKind::Body,
                "reading the response body failed: connection reset".to_owned()
            ),
        ]
    );
    let RelayErrorKind::Transport(t) = &errors[0].kind else {
        panic!()
    };
    let source = std::error::Error::source(t).unwrap();
    assert!(matches!(
        source.downcast_ref::<ServiceError>(),
        Some(ServiceError::Timeout)
    ));

    // A classifier refines the kind; `None` keeps the default.
    let transport = TowerTransport::new(mock).with_classifier(|e| {
        Some(match e.downcast_ref::<ServiceError>()? {
            ServiceError::Timeout => TransportErrorKind::Timeout,
            ServiceError::Refused => TransportErrorKind::Connect,
        })
    });
    let c = client(transport, &relays);
    let Error::Relays(errors) = c.fetch(10).await.unwrap_err() else {
        panic!()
    };
    assert!(matches!(errors[0].kind, RelayErrorKind::Timeout));
    let RelayErrorKind::Transport(t) = &errors[1].kind else {
        panic!()
    };
    assert_eq!(t.kind(), TransportErrorKind::Connect);
    let RelayErrorKind::Transport(t) = &errors[2].kind else {
        panic!()
    };
    assert_eq!(t.kind(), TransportErrorKind::Body);
}

#[test]
fn debug_names_the_service_without_requiring_debug() {
    let transport = TowerTransport::new(Mock::new(|_, _| Err("unused".into())));
    let debug = format!("{transport:?}");
    assert!(debug.starts_with("TowerTransport {"), "{debug}");
    assert!(debug.contains("Mock"), "{debug}");
}

// ---------------------------------------------------------------------------
// Over a real HTTP stack: tower-reqwest behind the README's `map_request`.

/// The stack the README shows for `tower-reqwest`, with an optional classifier.
fn over_reqwest(
    classify: Option<fn(&BoxError) -> Option<TransportErrorKind>>,
) -> impl HttpTransport {
    let service = ServiceBuilder::new()
        .map_request(|req: Request<Full<Bytes>>| req.map(reqwest::Body::wrap))
        .service(HttpClientService::new(reqwest::Client::new()));
    let transport = TowerTransport::new(service);
    match classify {
        Some(classify) => transport.with_classifier(classify),
        None => transport,
    }
}

/// A mock v1 relay: `handler(round)` is served after its delay. Returns the
/// base URL.
async fn relay(
    handler: impl Fn(u64) -> (Duration, axum::response::Response) + Clone + Send + Sync + 'static,
) -> String {
    let app = Router::new().route(
        "/{chain}/public/{round}",
        get(move |Path((_, round)): Path<(String, String)>| {
            let handler = handler.clone();
            async move {
                let (delay, response) = handler(round.parse().unwrap_or(0));
                tokio::time::sleep(delay).await;
                response
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

fn beacon(round: u64) -> (Duration, axum::response::Response) {
    (Duration::ZERO, beacon_json(round).into_response())
}

fn reqwest_client(transport: impl HttpTransport, relays: &[String]) -> Client {
    Client::builder(test_chain().chain().clone())
        .relays(relays.iter().map(|r| Relay::v1(r).unwrap()).collect())
        .http_transport(transport)
        .clock(FixedClock::at(due_ms(100) + 100))
        .request_timeout(Duration::from_millis(500))
        .build()
        .unwrap()
}

#[tokio::test]
async fn tower_reqwest_drives_fetch_and_latest() {
    let relay = relay(beacon).await;
    let c = reqwest_client(over_reqwest(None), &[relay]);
    let Fetch::Ready(b) = c.fetch(10).await.unwrap() else {
        panic!()
    };
    assert_eq!(b.round(), 10);
    assert_eq!(c.latest().await.unwrap().round(), 100);
}

#[tokio::test]
async fn tower_reqwest_caps_bodies_with_and_without_content_length() {
    // A `Vec<u8>` body carries `Content-Length`; a streamed one does not.
    let announced = relay(|_| {
        (
            Duration::ZERO,
            (StatusCode::OK, vec![b' '; MAX_BODY_LEN + 1]).into_response(),
        )
    })
    .await;
    let streamed = relay(|_| {
        let chunks = (0..20).map(|_| Ok::<_, std::io::Error>(vec![b' '; 8 * 1024]));
        (
            Duration::ZERO,
            axum::body::Body::from_stream(futures_util::stream::iter(chunks)).into_response(),
        )
    })
    .await;
    let c = reqwest_client(over_reqwest(None), &[announced, streamed]);
    let Error::Relays(errors) = c.fetch(10).await.unwrap_err() else {
        panic!()
    };
    assert_eq!(errors.len(), 2);
    assert!(matches!(errors[0].kind, RelayErrorKind::BodyTooLarge));
    assert!(matches!(errors[1].kind, RelayErrorKind::BodyTooLarge));
}

#[tokio::test]
async fn tower_reqwest_slow_relays_are_cut_off_by_the_client_timeout() {
    let slow = relay(|r| (Duration::from_secs(5), beacon_json(r).into_response())).await;
    let c = reqwest_client(over_reqwest(None), &[slow]);
    let Error::Relays(errors) = c.fetch(10).await.unwrap_err() else {
        panic!()
    };
    assert!(matches!(errors[0].kind, RelayErrorKind::Timeout));
}

#[tokio::test]
async fn tower_reqwest_errors_classify_by_downcast() {
    // Nothing listens on the discard port.
    let refused = ["http://127.0.0.1:9".to_owned()];

    // By default the kind is `Other`, and the reqwest error is the source.
    let c = reqwest_client(over_reqwest(None), &refused);
    let Error::Relays(errors) = c.fetch(10).await.unwrap_err() else {
        panic!()
    };
    let RelayErrorKind::Transport(t) = &errors[0].kind else {
        panic!("{:?}", errors[0].kind)
    };
    assert_eq!(t.kind(), TransportErrorKind::Other);
    let source = std::error::Error::source(t).unwrap();
    assert!(
        source
            .downcast_ref::<reqwest::Error>()
            .is_some_and(reqwest::Error::is_connect),
        "{source}"
    );

    // A classifier that downcasts to `reqwest::Error` tells `Connect` apart.
    let classify = |e: &BoxError| {
        let e = e.downcast_ref::<reqwest::Error>()?;
        Some(if e.is_connect() {
            TransportErrorKind::Connect
        } else if e.is_timeout() {
            TransportErrorKind::Timeout
        } else {
            TransportErrorKind::Other
        })
    };
    let c = reqwest_client(over_reqwest(Some(classify)), &refused);
    let Error::Relays(errors) = c.fetch(10).await.unwrap_err() else {
        panic!()
    };
    let RelayErrorKind::Transport(t) = &errors[0].kind else {
        panic!("{:?}", errors[0].kind)
    };
    assert_eq!(t.kind(), TransportErrorKind::Connect);
}
