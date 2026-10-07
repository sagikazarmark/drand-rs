//! Fetch and strictly verify [drand](https://drand.love) randomness beacons.
//!
//! The core of this crate is `no_std` (with `alloc`), never reads the clock and
//! runs anywhere, including `wasm32-unknown-unknown`:
//!
//! - [`ChainInfo`] and [`chains`]: chain parameters, pinned by hash.
//! - [`Schedule`] and [`UnixTime`]: round arithmetic.
//! - [`Beacon`] and [`Verifier`]: parsing (HTTP API v1 and v2) and strict BLS
//!   verification, producing a [`VerifiedBeacon`].
//! - [`http`]: relay URLs and response classification without doing any I/O,
//!   for bringing your own HTTP stack.
//!
//! With the `client` feature, `client::Client` fetches beacons from several
//! relays with hedged failover: on tokio on native targets, and on the JS
//! host's `fetch` and timers on `wasm32-unknown-unknown` (browsers, Cloudflare
//! Workers); not on WASI. Its built-in HTTP client uses reqwest's default TLS
//! backend (the default `default-tls` feature); pick another with `rustls` or
//! `native-tls`, or bring your own `reqwest::Client`.
//!
//! # Trust model
//!
//! A [`Verifier`] is built from a [`ChainInfo`], which is either built in
//! ([`chains`]), parsed against an expected chain hash
//! ([`ChainInfo::from_json`]), or constructed explicitly by the caller. Relays
//! are never trusted: every beacon is checked against the chain's public key,
//! and every encoding must be canonical, so a relay cannot choose between
//! several valid randomness values. What a relay can still do is withhold or
//! delay a beacon; applications must never move to a different round after a
//! failure, or a relay that dislikes a published round could force a redraw.
//!
//! ```
//! use drand::{Beacon, Verifier, chains};
//!
//! let verifier = Verifier::new(&chains::quicknet())?;
//! let beacon = Beacon::from_hex(
//!     1000,
//!     "b44679b9a59af2ec876b1a6b1ad52ea9b1615fc3982b19576350f93447cb1125e342b73a8dd2bacbe47e4b6b63ed5e39",
//!     None,
//! )?;
//! let verified = verifier.verify_round(1000, &beacon)?;
//! assert_eq!(
//!     hex::encode(verified.randomness()),
//!     "fe290beca10872ef2fb164d2aa4442de4566183ec51c56ff3cd603d930e54fdd",
//! );
//! # Ok::<(), drand::Error>(())
//! ```
//!
//! This crate has not been audited.

#![no_std]
#![cfg_attr(docsrs, feature(doc_cfg))]

extern crate alloc;
#[cfg(feature = "std")]
extern crate std;

mod beacon;
mod chain;
mod error;
pub mod http;
mod schedule;
#[cfg(feature = "test-chain")]
mod test_chain;
mod time;
mod verify;

#[cfg(all(
    feature = "client",
    any(not(target_arch = "wasm32"), target_os = "unknown")
))]
pub mod client;

pub use beacon::{Beacon, VerifiedBeacon};
pub use chain::{ChainHash, ChainInfo, Scheme, UnknownScheme, chains};
pub use error::Error;
pub use schedule::Schedule;
#[cfg(feature = "test-chain")]
pub use test_chain::TestChain;
pub use time::UnixTime;
pub use verify::Verifier;

pub(crate) fn sha256(parts: &[&[u8]]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize().into()
}
