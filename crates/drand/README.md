# drand

[![crates.io](https://img.shields.io/crates/v/drand?style=flat-square)](https://crates.io/crates/drand)
[![docs.rs](https://img.shields.io/docsrs/drand?style=flat-square)](https://docs.rs/drand)

**Fetch and strictly verify [drand](https://drand.love) randomness beacons in Rust.**

- A `no_std` + `alloc` core that never reads the clock and builds for `wasm32-unknown-unknown`: chain
  parameters, round arithmetic, v1/v2 beacon parsing and BLS verification.
- An async client (`client` feature; native targets on tokio, or `wasm32-unknown-unknown` without it) that asks several relays with hedged requests and
  returns the first beacon that verifies. Its single-attempt `fetch` fits durable executors (Restate,
  Temporal): "not yet" and "retryable" are explicit.
- A deterministic `TestChain` (`test-chain` feature) that signs real beacons, so tests go through the
  real verification path.

```rust
use drand::{Beacon, Verifier, chains};

let verifier = Verifier::new(&chains::quicknet())?;
let beacon: Beacon = serde_json::from_slice(br#"{"round":1000,"signature":"b44679b9a59af2ec876b1a6b1ad52ea9b1615fc3982b19576350f93447cb1125e342b73a8dd2bacbe47e4b6b63ed5e39"}"#)?;
let verified = verifier.verify_round(1000, &beacon)?;
println!("{}", hex::encode(verified.randomness()));
```

```rust
use std::time::Duration;
use drand::{chains, client::{Client, Fetch}};

let client = Client::builder(chains::quicknet()).build()?; // feature: reqwest
let latest = client.latest().await?;
let next = client.wait_for(latest.round() + 1, Duration::from_secs(30)).await?;
match client.fetch(next.round() + 1).await? {
    Fetch::Ready(beacon) => println!("{}", hex::encode(beacon.randomness())),
    Fetch::NotYet { due_at } => println!("due at {} (unix ms)", due_at.as_millis()),
    _ => {}
}
```

## Features

| Feature | Default | |
|---|---|---|
| `bls12-381` | yes | Verification of the BLS12-381 schemes (`default`, `quicknet`) via arkworks |
| `std` | no | `From<std::time::SystemTime>` for `UnixTime` (implied by `client`) |
| `client` | no | Async multi-relay client: tokio on native targets, the JS host's `fetch` and `setTimeout` on `wasm32-unknown-unknown` (cfg'd out on WASI). Without `reqwest`, `http_transport` is mandatory |
| `reqwest` | no | The default transport, `ReqwestTransport` (implies `client`); the only place `reqwest` appears |
| `default-tls` | yes | TLS for `ReqwestTransport`: reqwest's default backend (rustls) |
| `rustls` / `native-tls` | no | A specific TLS backend for `ReqwestTransport` |
| `tower` | no | `TowerTransport`: an `HttpTransport` over any `tower::Service` (implies `client`); `tower-reqwest`, `tower-worker-fetch` on Cloudflare Workers, or your own stack |
| `test-chain` | no | `TestChain`: signs beacons with a key derived from a public seed |

The TLS features only matter with `reqwest`; the core stays `no_std` either way. The usual setup is `features =
["reqwest"]` (with the default `default-tls`). To choose a TLS backend, set `default-features = false` and
enable `bls12-381`, `reqwest` and `rustls` or `native-tls`. To keep reqwest out of the build entirely, enable
`client` alone and bring your own transport (below). On `wasm32-unknown-unknown` the JS host does TLS and the
TLS features do nothing.

### Bring your own HTTP client

The client sends requests through the `drand::client::HttpTransport` trait: one `http::Request<Bytes>` in, one
buffered `http::Response<Bytes>` out. Relay selection, hedging, timeouts, response classification and
verification all happen above it. The default transport, `ReqwestTransport` (feature `reqwest`), is `reqwest`
without its other default features, so it ignores `HTTPS_PROXY`; wrap your own `reqwest::Client` if you need
a proxy or custom TLS roots:

```rust
use drand::client::{Client, ReqwestTransport};

let http = reqwest::Client::builder().build()?;
let client = Client::builder(drand::chains::quicknet())
    .http_transport(ReqwestTransport::new(http))
    .build()?;
```

For another backend (a JS host's `fetch`, a `tower` service, an in-memory fake in tests), implement
`HttpTransport` yourself and enable `client` without `reqwest`: reqwest is then not compiled at all, and
`build()` fails with `Error::Config` if no transport is set. `ReqwestTransport::new` is the only place `reqwest`
appears in this crate's API, so a `reqwest` upgrade only affects you if you pass one in.

#### tower services

With the `tower` feature, `TowerTransport` adapts any `tower::Service` that takes an `http::Request<Full<Bytes>>`
and returns an `http::Response` with a streaming `http_body::Body`: `tower-reqwest` on native targets,
`tower-worker-fetch` on Cloudflare Workers, or a `ServiceBuilder` stack over either. It clones the service per
request, caps the body at the client's limit while reading, and reports the service's errors as
`TransportErrorKind::Other` (body read errors as `Body`) with the original error as the source. Nothing
`tower-worker-fetch`-specific is in this crate; it is an ordinary service:

```rust
use drand::client::{Client, TowerTransport};
use tower_worker_fetch::FetchService;

let client = Client::builder(drand::chains::quicknet())
    .http_transport(TowerTransport::new(FetchService::new()))
    .build()?;
```

A classifier refines the error kind for a service whose error type says more (only diagnostics depend on it;
the client retries on the same conditions whatever the kind):

```rust
use drand::client::TransportErrorKind;

let transport = TowerTransport::new(FetchService::new()).with_classifier(|e| {
    let e = e.downcast_ref::<tower_worker_fetch::Error>()?;
    Some(if e.is_timeout() { TransportErrorKind::Timeout } else { TransportErrorKind::Other })
});
```

No `tower::timeout` layer is needed: the client enforces its per-request timeout above the transport (and on
Workers, where there is no tokio, `tower::timeout` would panic). Hedging and retries stay in the client too.

The client, `HttpTransport` and their futures are `Send` on every target, `wasm32` included, so they drop into
`Send`-requiring frameworks (tower, axum, Restate) on Cloudflare Workers too. JS futures are not `Send`, but
`wasm32-unknown-unknown` is single-threaded: the client wraps its own, and a transport over a JS API wraps
its with `drand::client::assume_send`.

Supported schemes: `pedersen-bls-chained`, `pedersen-bls-unchained`, `bls-unchained-g1-rfc9380`.
`bls-bn254-unchained-on-g1` (evmnet) is parsed but not verified yet; the deprecated `bls-unchained-on-g1`
is reported as unsupported. MSRV: 1.92.

## Tests

`cargo test -p drand --features reqwest,tower,test-chain` runs everything offline: real beacons from the live relays are checked
in under `tests/data`, and client behavior is tested against in-process mock relays and services. The live tests talk to
every public relay and API version separately (chain info, quicknet and `default` rounds, a future round) and
are opt-in:

```sh
cargo test -p drand --features reqwest --test live -- --ignored
```

## Security

- **Trust root.** A `Verifier` is built from a `ChainInfo` that is built in (`chains::*`), parsed against an
  expected chain hash (`ChainInfo::from_json`), or constructed explicitly. Relay-supplied `/info` is never
  trusted by itself. drand's chain hash does not cover the scheme, but a mislabeled scheme can only make
  verification fail: the network signs one message per round, and no other scheme's message equals it
  without a SHA-256 collision.
- **Checks.** Every beacon is verified with a pairing check against the chain key. Points must use the one
  canonical encoding (exact length, canonical flags, coordinates below the field modulus, not infinity, in
  the prime-order subgroup), because the randomness is `sha256(signature bytes)`: two encodings of one point
  would let a relay choose between two "verified" values. Chained beacons must carry the right previous
  signature (the genesis seed at round 1); unchained beacons drop any previous signature. The round is part
  of the signed message, and `verify_round` also checks it against the requested round. Round 0 is rejected
  (relays serve the latest beacon for it).
- **What a relay can still do.** Withhold, delay, or answer "not found" for a round that is due. That only
  affects liveness, *if your application never moves to a different round after a failure*. Otherwise a
  relay that dislikes a published round can force a redraw. `fetch` reports such failures as retryable.
- **drand's own assumption.** A threshold of colluding League of Entropy nodes can learn rounds early. When
  committing to a future round (`Schedule::round_after`), leave at least one period of margin.
- **Not audited.**

## Compared with other crates

| | drand | [`drand_core`](https://crates.io/crates/drand_core) | [`drand-verify`](https://crates.io/crates/drand-verify) |
|---|---|---|---|
| Verification | strict (canonical encodings) | yes | yes |
| Networking | async, several relays, hedged; or bring your own via `http` | blocking `ureq`, one relay | none |
| HTTP API | v1 and v2 | v1 | — |
| `no_std` / wasm core | yes / yes | no / yes | no / yes |
| Test chain that signs | yes | no | no |
| evmnet (BN254) | planned | no | no |

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
