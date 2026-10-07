//! drand's HTTP API without the I/O: relay URLs and response classification.
//!
//! Use this module to fetch beacons with any HTTP stack (in the browser, in a
//! durable executor's step, …), then verify them with a [`crate::Verifier`].
//! The `client` feature builds an async client on top of it.

use alloc::{
    borrow::ToOwned,
    format,
    string::{String, ToString},
    vec::Vec,
};
use core::{fmt, num::NonZeroU64, str::FromStr, time::Duration};

use crate::{Beacon, ChainHash, Error, Schedule, UnixTime, chains};

/// The largest response body a relay may send, in bytes.
pub const MAX_BODY_LEN: usize = 64 * 1024;

/// How long after a round is due a `404` still means "not yet" (relays lag).
pub const GRACE: Duration = Duration::from_secs(2);

/// Relay API flavour.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ApiVersion {
    /// `{base}/{chain}/public/{round}` (drand.sh, Cloudflare).
    V1,
    /// `{base}/v2/chains/{chain}/rounds/{round}` (drand.sh).
    V2,
}

/// Which round to ask for.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoundRef {
    /// A specific round (round 0 means "latest" to relays, so it cannot be expressed).
    Number(NonZeroU64),
    /// Whatever the relay has as latest (may be cached for a couple of seconds).
    Latest,
}

/// A relay endpoint.
///
/// Parses from strings like `https://api.drand.sh` (v1), `v1:https://…` or
/// `v2:https://api.drand.sh`, and displays in the prefixed form.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Relay {
    base: String,
    version: ApiVersion,
}

impl Relay {
    fn new(base: &str, version: ApiVersion) -> Result<Self, Error> {
        let base = base.trim().trim_end_matches('/');
        let rest = base
            .strip_prefix("https://")
            .or_else(|| base.strip_prefix("http://"))
            .ok_or(Error::parse("relay URL"))?;
        let host = rest.split('/').next().unwrap_or_default();
        let bad = |c: char| c.is_ascii_control() || c.is_whitespace() || "?#@\\".contains(c);
        // No credentials (they would end up in logs), no query or fragment.
        if host.is_empty() || rest.contains(bad) {
            return Err(Error::parse("relay URL"));
        }
        let port = match host.rsplit_once(']') {
            Some((_, after)) => after.strip_prefix(':'),
            None => host.split_once(':').map(|(_, p)| p),
        };
        if port.is_some_and(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit())) {
            return Err(Error::parse("relay URL"));
        }
        Ok(Self {
            base: base.to_owned(),
            version,
        })
    }

    /// Stands for the client's in-process test transport in errors.
    #[cfg(feature = "test-chain")]
    #[allow(
        dead_code,
        reason = "used by the client, which is not built everywhere"
    )]
    pub(crate) fn test_chain() -> Self {
        Self {
            base: "test-chain".to_owned(),
            version: ApiVersion::V1,
        }
    }

    /// A v1 relay at `base` (`http://` or `https://`).
    ///
    /// # Errors
    ///
    /// [`Error::Parse`] for a URL with another scheme, no host, credentials,
    /// a query or fragment, or an invalid port.
    pub fn v1(base: &str) -> Result<Self, Error> {
        Self::new(base, ApiVersion::V1)
    }

    /// A v2 relay at `base` (`http://` or `https://`).
    ///
    /// # Errors
    ///
    /// As [`Relay::v1`].
    pub fn v2(base: &str) -> Result<Self, Error> {
        Self::new(base, ApiVersion::V2)
    }

    /// The base URL, without a trailing slash.
    #[must_use]
    pub fn base(&self) -> &str {
        &self.base
    }

    /// The API version.
    #[must_use]
    pub fn version(&self) -> ApiVersion {
        self.version
    }

    /// The URL of the chain's `/info`.
    #[must_use]
    pub fn info_url(&self, chain: &ChainHash) -> String {
        match self.version {
            ApiVersion::V1 => format!("{}/{chain}/info", self.base),
            ApiVersion::V2 => format!("{}/v2/chains/{chain}/info", self.base),
        }
    }

    /// The URL of a round.
    #[must_use]
    pub fn round_url(&self, chain: &ChainHash, round: RoundRef) -> String {
        let r = match round {
            RoundRef::Number(n) => n.to_string(),
            RoundRef::Latest => "latest".to_owned(),
        };
        match self.version {
            ApiVersion::V1 => format!("{}/{chain}/public/{r}", self.base),
            ApiVersion::V2 => format!("{}/v2/chains/{chain}/rounds/{r}", self.base),
        }
    }
}

impl fmt::Display for Relay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let v = match self.version {
            ApiVersion::V1 => "v1",
            ApiVersion::V2 => "v2",
        };
        write!(f, "{v}:{}", self.base)
    }
}

impl FromStr for Relay {
    type Err = Error;

    /// `v1:URL`, `v2:URL`, or a bare URL (v1, which every relay serves).
    fn from_str(s: &str) -> Result<Self, Error> {
        let s = s.trim();
        if let Some(url) = s.strip_prefix("v2:") {
            Self::v2(url)
        } else if let Some(url) = s.strip_prefix("v1:") {
            Self::v1(url)
        } else {
            Self::v1(s)
        }
    }
}

/// Well-known public relays.
pub mod relays {
    use super::{ChainHash, Relay, Vec, chains};

    /// A League of Entropy relay (v1 and v2).
    pub const API_DRAND_SH: &str = "https://api.drand.sh";
    /// A League of Entropy relay (v1 and v2).
    pub const API2_DRAND_SH: &str = "https://api2.drand.sh";
    /// A League of Entropy relay (v1 and v2).
    pub const API3_DRAND_SH: &str = "https://api3.drand.sh";
    /// Cloudflare's relay (v1 only).
    pub const CLOUDFLARE: &str = "https://drand.cloudflare.com";

    /// Default relays for a built-in chain, in the order to try them; empty
    /// for other chains.
    #[must_use]
    #[expect(
        clippy::missing_panics_doc,
        reason = "the built-in relay URLs are valid"
    )]
    pub fn defaults(chain: &ChainHash) -> Vec<Relay> {
        if chains::by_hash(chain).is_none() {
            return Vec::new();
        }
        [
            Relay::v2(API_DRAND_SH),
            Relay::v2(API2_DRAND_SH),
            Relay::v2(API3_DRAND_SH),
            Relay::v1(CLOUDFLARE),
        ]
        .into_iter()
        .map(|r| r.expect("built-in relay"))
        .collect()
    }
}

/// What one relay response means.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    /// A beacon, not yet verified.
    Beacon(Beacon),
    /// The round is not published yet (425, or 404 until [`GRACE`] after the
    /// round is due).
    NotYet,
    /// The relay does not have the round (404 well after it was due).
    NotFound,
    /// A transient failure (5xx, 429): try again or try another relay.
    Transient,
    /// Any other status.
    Unexpected(u16),
}

/// Classifies one relay response. `now` is when the request was sent.
///
/// - 2xx: the body is parsed; for a numbered request the round must match.
/// - 425: [`Answer::NotYet`] (whatever the time; the caller decides what a late "not yet" means).
/// - 404: [`Answer::NotYet`] until [`GRACE`] after the round is due (Cloudflare
///   answers 404 for future rounds), then [`Answer::NotFound`].
/// - 5xx and 429: [`Answer::Transient`].
/// - Anything else: [`Answer::Unexpected`].
///
/// # Errors
///
/// For a 2xx response: [`Error::Length`] for a body over [`MAX_BODY_LEN`],
/// [`Error::Parse`] for a malformed beacon, [`Error::RoundMismatch`] for the
/// wrong round, and [`Error::RoundZero`] for round 0.
pub fn classify(
    req: RoundRef,
    status: u16,
    body: &[u8],
    schedule: &Schedule,
    now: UnixTime,
) -> Result<Answer, Error> {
    Ok(match status {
        200..=299 => {
            if body.len() > MAX_BODY_LEN {
                return Err(Error::length("response body", MAX_BODY_LEN, body.len()));
            }
            let beacon: Beacon =
                serde_json::from_slice(body).map_err(|_| Error::parse("beacon"))?;
            match req {
                RoundRef::Number(n) if beacon.round != n.get() => {
                    return Err(Error::RoundMismatch {
                        expected: n.get(),
                        got: beacon.round,
                    });
                }
                _ if beacon.round == 0 => return Err(Error::RoundZero),
                _ => Answer::Beacon(beacon),
            }
        }
        425 => Answer::NotYet,
        404 => match req {
            RoundRef::Number(n) => match schedule.round_time(n.get()) {
                Some(due) if now >= due.saturating_add(GRACE) => Answer::NotFound,
                _ => Answer::NotYet,
            },
            RoundRef::Latest => Answer::NotFound,
        },
        429 | 500..=599 => Answer::Transient,
        other => Answer::Unexpected(other),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(r: u64) -> RoundRef {
        RoundRef::Number(NonZeroU64::new(r).unwrap())
    }

    #[test]
    fn urls() {
        let q = chains::QUICKNET_HASH;
        let v1: Relay = "https://drand.cloudflare.com/".parse().unwrap();
        let v2: Relay = "v2:https://api.drand.sh".parse().unwrap();
        assert_eq!(
            v1.round_url(&q, n(7)),
            format!("https://drand.cloudflare.com/{q}/public/7")
        );
        assert_eq!(
            v2.round_url(&q, RoundRef::Latest),
            format!("https://api.drand.sh/v2/chains/{q}/rounds/latest")
        );
        assert_eq!(
            v2.info_url(&q),
            format!("https://api.drand.sh/v2/chains/{q}/info")
        );
        assert_eq!(v2.to_string(), "v2:https://api.drand.sh");
        assert!("ftp://x".parse::<Relay>().is_err());
        for bad in [
            "https://user:pw@host",
            "https://host:port",
            "https://host:",
            "https://a\nb",
            "https://a\tb",
            "https://a?x=1",
        ] {
            assert!(bad.parse::<Relay>().is_err(), "{bad:?}");
        }
        assert!("http://127.0.0.1:9".parse::<Relay>().is_ok());
        assert!("http://[::1]:8080/x".parse::<Relay>().is_ok());
        assert!("https://".parse::<Relay>().is_err());
        assert_eq!(relays::defaults(&q).len(), 4);
        assert_eq!(relays::defaults(&ChainHash::from_bytes([0; 32])), []);
    }

    #[test]
    fn classification() {
        let s = chains::quicknet().schedule();
        let due = s.round_time(100).unwrap();
        let ms = |d: i64| UnixTime::from_millis(due.as_millis() + d);
        let grace = i64::try_from(GRACE.as_millis()).unwrap();
        assert_eq!(
            classify(n(100), 425, b"", &s, ms(60_000)),
            Ok(Answer::NotYet)
        );
        assert_eq!(classify(n(100), 404, b"", &s, ms(-1)), Ok(Answer::NotYet));
        assert_eq!(
            classify(n(100), 404, b"", &s, ms(grace - 1)),
            Ok(Answer::NotYet)
        );
        assert_eq!(
            classify(n(100), 404, b"", &s, ms(grace)),
            Ok(Answer::NotFound)
        );
        assert_eq!(classify(n(100), 503, b"", &s, due), Ok(Answer::Transient));
        assert_eq!(classify(n(100), 429, b"", &s, due), Ok(Answer::Transient));
        assert_eq!(
            classify(n(100), 403, b"", &s, due),
            Ok(Answer::Unexpected(403))
        );
        let body = br#"{"round":101,"signature":"aa"}"#;
        assert_eq!(
            classify(n(100), 200, body, &s, due),
            Err(Error::RoundMismatch {
                expected: 100,
                got: 101
            })
        );
        assert!(matches!(
            classify(RoundRef::Latest, 200, body, &s, due),
            Ok(Answer::Beacon(_))
        ));
        assert_eq!(
            classify(
                RoundRef::Latest,
                200,
                br#"{"round":0,"signature":"aa"}"#,
                &s,
                due
            ),
            Err(Error::RoundZero)
        );
        let big = alloc::vec![b' '; MAX_BODY_LEN + 1];
        assert!(classify(n(100), 200, &big, &s, due).is_err());
    }
}
