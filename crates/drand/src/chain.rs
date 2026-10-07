use alloc::{borrow::ToOwned, string::String, vec::Vec};
use core::{fmt, num::NonZeroU32, str::FromStr, time::Duration};

use serde::{Deserialize, Deserializer, Serialize, Serializer, ser::SerializeStruct};

use crate::{Error, Schedule, UnixTime, sha256};

/// A drand signature scheme.
///
/// Every scheme signs a hash of the round number (`round`: big-endian `u64`);
/// the randomness is always `sha256(signature)`.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Scheme {
    /// `pedersen-bls-chained` (the `default` chain): BLS12-381, public key on
    /// G1 (48 bytes), signatures on G2 (96 bytes) over
    /// `sha256(previous_signature ‖ round)`.
    PedersenBlsChained,
    /// `pedersen-bls-unchained`: BLS12-381, public key on G1 (48 bytes),
    /// signatures on G2 (96 bytes) over `sha256(round)`.
    PedersenBlsUnchained,
    /// `bls-unchained-g1-rfc9380` (`quicknet`): BLS12-381, public key on G2
    /// (96 bytes), signatures on G1 (48 bytes) over `sha256(round)`, hashed to
    /// G1 with the RFC 9380 tag `BLS_SIG_BLS12381G1_XMD:SHA-256_SSWU_RO_NUL_`.
    BlsUnchainedG1Rfc9380,
    /// `bls-bn254-unchained-on-g1` (`evmnet`): BN254, public key on G2
    /// (128 bytes, uncompressed), signatures on G1 over `keccak256(round)`,
    /// with the tag `BLS_SIG_BN254G1_XMD:KECCAK-256_SVDW_RO_NUL_`. Parsed, not
    /// verifiable yet.
    BlsBn254UnchainedOnG1,
    /// `bls-unchained-on-g1`, deprecated by drand in favour of
    /// [`Scheme::BlsUnchainedG1Rfc9380`]: the same groups, but hashed to G1
    /// with the G2 tag `BLS_SIG_BLS12381G2_XMD:SHA-256_SSWU_RO_NUL_`, against
    /// RFC 9380. Parsed, never verified.
    BlsUnchainedOnG1,
    /// Any other scheme ID.
    Other(UnknownScheme),
}

/// A scheme ID this crate does not know.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct UnknownScheme(String);

impl UnknownScheme {
    /// The scheme ID.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Scheme {
    /// The scheme ID as used by drand.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::PedersenBlsChained => "pedersen-bls-chained",
            Self::PedersenBlsUnchained => "pedersen-bls-unchained",
            Self::BlsUnchainedG1Rfc9380 => "bls-unchained-g1-rfc9380",
            Self::BlsBn254UnchainedOnG1 => "bls-bn254-unchained-on-g1",
            Self::BlsUnchainedOnG1 => "bls-unchained-on-g1",
            Self::Other(s) => s.as_str(),
        }
    }

    /// Parses a scheme ID; unknown IDs become [`Scheme::Other`].
    #[must_use]
    pub fn parse(id: &str) -> Self {
        match id {
            "pedersen-bls-chained" => Self::PedersenBlsChained,
            "pedersen-bls-unchained" => Self::PedersenBlsUnchained,
            "bls-unchained-g1-rfc9380" => Self::BlsUnchainedG1Rfc9380,
            "bls-bn254-unchained-on-g1" => Self::BlsBn254UnchainedOnG1,
            "bls-unchained-on-g1" => Self::BlsUnchainedOnG1,
            other => Self::Other(UnknownScheme(other.to_owned())),
        }
    }

    /// Whether each signature also signs the previous one.
    #[must_use]
    pub fn is_chained(&self) -> bool {
        matches!(self, Self::PedersenBlsChained)
    }

    /// Length of the group public key, for known schemes.
    pub(crate) fn public_key_len(&self) -> Option<usize> {
        match self {
            Self::PedersenBlsChained | Self::PedersenBlsUnchained => Some(48),
            Self::BlsUnchainedG1Rfc9380 | Self::BlsUnchainedOnG1 => Some(96),
            Self::BlsBn254UnchainedOnG1 => Some(128),
            Self::Other(_) => None,
        }
    }
}

impl fmt::Display for Scheme {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Scheme {
    type Err = core::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self::parse(s))
    }
}

impl Serialize for Scheme {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Scheme {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let id = String::deserialize(d)?;
        Ok(Self::parse(&id))
    }
}

/// The hash identifying a drand chain.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ChainHash([u8; 32]);

impl ChainHash {
    /// Wraps raw hash bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The raw hash bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Parses a hex chain hash at compile time (panics on invalid input).
    const fn from_hex_const(s: &str) -> Self {
        const fn nibble(c: u8) -> u8 {
            match c {
                b'0'..=b'9' => c - b'0',
                b'a'..=b'f' => c - b'a' + 10,
                _ => panic!("invalid hex"),
            }
        }
        let s = s.as_bytes();
        assert!(s.len() == 64);
        let mut out = [0u8; 32];
        let mut i = 0;
        while i < 32 {
            out[i] = nibble(s[2 * i]) << 4 | nibble(s[2 * i + 1]);
            i += 1;
        }
        Self(out)
    }
}

impl AsRef<[u8]> for ChainHash {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Display for ChainHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for ChainHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ChainHash({self})")
    }
}

impl FromStr for ChainHash {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Error> {
        let mut out = [0u8; 32];
        hex::decode_to_slice(s, &mut out).map_err(|_| Error::parse("chain hash"))?;
        Ok(Self(out))
    }
}

impl Serialize for ChainHash {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if s.is_human_readable() {
            s.collect_str(self)
        } else {
            self.0.serialize(s)
        }
    }
}

impl<'de> Deserialize<'de> for ChainHash {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        if d.is_human_readable() {
            let s = String::deserialize(d)?;
            s.parse().map_err(serde::de::Error::custom)
        } else {
            <[u8; 32]>::deserialize(d).map(Self)
        }
    }
}

/// The parameters of a drand chain.
///
/// There is deliberately no `Deserialize`: a `ChainInfo` comes from
/// [`chains`], from [`ChainInfo::from_json`] against an expected hash, or from
/// [`ChainInfo::new`], where the caller vouches for the parameters. Relays are
/// never trusted to supply it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainInfo {
    public_key: Vec<u8>,
    period: NonZeroU32,
    genesis_time: i64,
    genesis_seed: Vec<u8>,
    scheme: Scheme,
    beacon_id: String,
}

impl ChainInfo {
    /// Chain parameters from trusted values, with `period` and `genesis_time`
    /// in seconds as drand publishes (and hashes) them. An empty `beacon_id`
    /// means `"default"`.
    pub fn new(
        public_key: Vec<u8>,
        period: NonZeroU32,
        genesis_time: i64,
        genesis_seed: Vec<u8>,
        scheme: Scheme,
        beacon_id: impl Into<String>,
    ) -> Self {
        let mut beacon_id = beacon_id.into();
        if beacon_id.is_empty() {
            beacon_id = "default".into();
        }
        Self {
            public_key,
            period,
            genesis_time,
            genesis_seed,
            scheme,
            beacon_id,
        }
    }

    /// Parses a v1 or v2 `/info` response and checks that it hashes to `expected`.
    ///
    /// The hash in the body is ignored; the hash is always computed. The chain
    /// hash does not cover the scheme, but a mislabeled scheme can only make
    /// verification fail, never accept wrong randomness.
    ///
    /// # Errors
    ///
    /// [`Error::Parse`] for a malformed body or one that differs from the
    /// built-in chain with that hash, [`Error::Length`] for a genesis seed or
    /// public key of the wrong length, and [`Error::ChainHashMismatch`] if it
    /// does not hash to `expected`.
    pub fn from_json(body: &[u8], expected: &ChainHash) -> Result<Self, Error> {
        #[derive(Deserialize)]
        struct Meta {
            #[serde(rename = "beaconID")]
            beacon_id: Option<String>,
        }
        #[derive(Deserialize)]
        struct Info {
            public_key: String,
            period: u32,
            genesis_time: i64,
            #[serde(alias = "groupHash")]
            genesis_seed: String,
            #[serde(alias = "schemeID")]
            scheme: Option<String>,
            beacon_id: Option<String>,
            metadata: Option<Meta>,
        }
        let info: Info = serde_json::from_slice(body).map_err(|_| Error::parse("chain info"))?;
        let period = NonZeroU32::new(info.period).ok_or(Error::parse("period"))?;
        let public_key = hex::decode(&info.public_key).map_err(|_| Error::parse("public key"))?;
        let genesis_seed =
            hex::decode(&info.genesis_seed).map_err(|_| Error::parse("genesis seed"))?;
        let beacon_id = info
            .beacon_id
            .or(info.metadata.and_then(|m| m.beacon_id))
            .unwrap_or_default();
        let scheme = Scheme::parse(info.scheme.as_deref().unwrap_or("pedersen-bls-chained"));
        let chain = Self::new(
            public_key,
            period,
            info.genesis_time,
            genesis_seed,
            scheme,
            beacon_id,
        );
        // The hash concatenates key, seed and beacon ID without lengths, so
        // fix the lengths: a relay must not move bytes between the fields.
        if chain.genesis_seed.len() != 32 {
            return Err(Error::length("genesis seed", 32, chain.genesis_seed.len()));
        }
        if let Some(len) = chain
            .scheme
            .public_key_len()
            .filter(|len| *len != chain.public_key.len())
        {
            return Err(Error::length("public key", len, chain.public_key.len()));
        }
        let got = chain.hash();
        if got != *expected {
            return Err(Error::ChainHashMismatch {
                expected: *expected,
                got,
            });
        }
        // A built-in chain must match exactly (scheme and beacon ID included).
        if chains::by_hash(expected).is_some_and(|known| known != chain) {
            return Err(Error::parse("chain info (differs from the built-in chain)"));
        }
        Ok(chain)
    }

    /// The chain hash, computed as drand does:
    /// `sha256(u32be(period) ‖ i64be(genesis_time) ‖ public_key ‖ genesis_seed ‖ beacon_id)`,
    /// where `beacon_id` is omitted for `"default"`. The scheme is not part of it.
    #[must_use]
    pub fn hash(&self) -> ChainHash {
        let id: &[u8] = if self.beacon_id == "default" {
            &[]
        } else {
            self.beacon_id.as_bytes()
        };
        ChainHash(sha256(&[
            &self.period.get().to_be_bytes(),
            &self.genesis_time.to_be_bytes(),
            &self.public_key,
            &self.genesis_seed,
            id,
        ]))
    }

    /// Round timing.
    #[must_use]
    pub fn schedule(&self) -> Schedule {
        Schedule::new(self.genesis_time, self.period)
    }

    /// The group public key (compressed point encoding).
    #[must_use]
    pub fn public_key(&self) -> &[u8] {
        &self.public_key
    }

    /// Time between rounds.
    #[must_use]
    pub fn period(&self) -> Duration {
        Duration::from_secs(u64::from(self.period.get()))
    }

    /// When round 1 is due.
    #[must_use]
    pub fn genesis_time(&self) -> UnixTime {
        UnixTime::from_secs(self.genesis_time)
    }

    /// The genesis seed (the "previous signature" of round 1 on chained schemes).
    #[must_use]
    pub fn genesis_seed(&self) -> &[u8] {
        &self.genesis_seed
    }

    /// The signature scheme.
    #[must_use]
    pub fn scheme(&self) -> &Scheme {
        &self.scheme
    }

    /// The beacon ID (`"default"`, `"quicknet"`, …).
    #[must_use]
    pub fn beacon_id(&self) -> &str {
        &self.beacon_id
    }
}

/// Serializes in the v2 `/info` shape, with the computed `chain_hash`.
impl Serialize for ChainInfo {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut st = s.serialize_struct("ChainInfo", 7)?;
        st.serialize_field("public_key", &hex::encode(&self.public_key))?;
        st.serialize_field("period", &self.period.get())?;
        st.serialize_field("genesis_time", &self.genesis_time)?;
        st.serialize_field("genesis_seed", &hex::encode(&self.genesis_seed))?;
        st.serialize_field("chain_hash", &self.hash())?;
        st.serialize_field("scheme", &self.scheme)?;
        st.serialize_field("beacon_id", &self.beacon_id)?;
        st.end()
    }
}

/// The League of Entropy mainnet chains, fully pinned (scheme included).
///
/// All three are mainnet; `default` is the beacon ID of the oldest one.
pub mod chains {
    use super::{ChainHash, ChainInfo, NonZeroU32, Scheme};

    /// Hash of the `quicknet` chain (3s, `bls-unchained-g1-rfc9380`).
    pub const QUICKNET_HASH: ChainHash = ChainHash::from_hex_const(
        "52db9ba70e0cc0f6eaf7803dd07447a1f5477735fd3f661792ba94600c84e971",
    );
    /// Hash of the `default` chain (30s, `pedersen-bls-chained`).
    pub const DEFAULT_HASH: ChainHash = ChainHash::from_hex_const(
        "8990e7a9aaed2ffed73dbd7092123d6f289930540d7651336225dc172e51b2ce",
    );
    /// Hash of the `evmnet` chain (3s, `bls-bn254-unchained-on-g1`).
    pub const EVMNET_HASH: ChainHash = ChainHash::from_hex_const(
        "04f1e9062b8a81f848fded9c12306733282b2727ecced50032187751166ec8c3",
    );

    fn chain(
        public_key: &str,
        period: u32,
        genesis_time: i64,
        genesis_seed: &str,
        scheme: Scheme,
        beacon_id: &str,
    ) -> ChainInfo {
        ChainInfo::new(
            hex::decode(public_key).expect("built-in key"),
            NonZeroU32::new(period).expect("built-in period"),
            genesis_time,
            hex::decode(genesis_seed).expect("built-in seed"),
            scheme,
            beacon_id,
        )
    }

    /// The `quicknet` chain.
    #[must_use]
    pub fn quicknet() -> ChainInfo {
        chain(
            "83cf0f2896adee7eb8b5f01fcad3912212c437e0073e911fb90022d3e760183c8c4b450b6a0a6c3ac6a5776a2d1064510d1fec758c921cc22b0e17e63aaf4bcb5ed66304de9cf809bd274ca73bab4af5a6e9c76a4bc09e76eae8991ef5ece45a",
            3,
            1_692_803_367,
            "f477d5c89f21a17c863a7f937c6a6d15859414d2be09cd448d4279af331c5d3e",
            Scheme::BlsUnchainedG1Rfc9380,
            "quicknet",
        )
    }

    /// The `default` chain.
    #[must_use]
    pub fn default() -> ChainInfo {
        chain(
            "868f005eb8e6e4ca0a47c8a77ceaa5309a47978a7c71bc5cce96366b5d7a569937c529eeda66c7293784a9402801af31",
            30,
            1_595_431_050,
            "176f93498eac9ca337150b46d21dd58673ea4e3581185f869672e59fa4cb390a",
            Scheme::PedersenBlsChained,
            "default",
        )
    }

    /// The `evmnet` chain. Its BN254 signatures cannot be verified by this version.
    #[must_use]
    pub fn evmnet() -> ChainInfo {
        chain(
            "07e1d1d335df83fa98462005690372c643340060d205306a9aa8106b6bd0b3820557ec32c2ad488e4d4f6008f89a346f18492092ccc0d594610de2732c8b808f0095685ae3a85ba243747b1b2f426049010f6b73a0cf1d389351d5aaaa1047f6297d3a4f9749b33eb2d904c9d9ebf17224150ddd7abd7567a9bec6c74480ee0b",
            3,
            1_727_521_075,
            "cd7ad2f0e0cce5d8c288f2dd016ffe7bc8dc88dbb229b3da2b6ad736490dfed6",
            Scheme::BlsBn254UnchainedOnG1,
            "evmnet",
        )
    }

    /// A built-in chain by hash.
    #[must_use]
    pub fn by_hash(hash: &ChainHash) -> Option<ChainInfo> {
        match *hash {
            QUICKNET_HASH => Some(quicknet()),
            DEFAULT_HASH => Some(default()),
            EVMNET_HASH => Some(evmnet()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_chains_hash_to_their_constants() {
        assert_eq!(chains::quicknet().hash(), chains::QUICKNET_HASH);
        assert_eq!(chains::default().hash(), chains::DEFAULT_HASH);
        assert_eq!(chains::evmnet().hash(), chains::EVMNET_HASH);
    }

    #[test]
    fn chain_hash_round_trips_as_hex() {
        let s = "52db9ba70e0cc0f6eaf7803dd07447a1f5477735fd3f661792ba94600c84e971";
        let h: ChainHash = s.parse().unwrap();
        assert_eq!(h, chains::QUICKNET_HASH);
        assert_eq!(alloc::format!("{h}"), s);
        assert!("zz".parse::<ChainHash>().is_err());
    }

    #[test]
    fn schemes_round_trip() {
        for id in [
            "pedersen-bls-chained",
            "pedersen-bls-unchained",
            "bls-unchained-g1-rfc9380",
            "bls-bn254-unchained-on-g1",
            "bls-unchained-on-g1",
            "something-new",
        ] {
            assert_eq!(Scheme::parse(id).as_str(), id);
        }
        assert!(matches!(Scheme::parse("x"), Scheme::Other(_)));
    }
}
