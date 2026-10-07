use alloc::vec::Vec;

use crate::{Beacon, ChainHash, ChainInfo, Error, Scheme, VerifiedBeacon, sha256};

/// Verifies beacons of one chain.
///
/// Built once from a [`ChainInfo`] (decoding and checking the public key), then
/// cheap to use: one pairing check per beacon.
#[derive(Clone, Debug)]
pub struct Verifier {
    chain_hash: ChainHash,
    scheme: Scheme,
    genesis_seed: Vec<u8>,
    #[cfg_attr(not(feature = "bls12-381"), allow(dead_code))]
    key: Key,
}

#[derive(Clone, Debug)]
enum Key {
    /// Key on G1, signatures on G2 (pedersen schemes).
    #[cfg(feature = "bls12-381")]
    Bls12381G1(ark_bls12_381::G1Affine),
    /// Key on G2, signatures on G1 (quicknet).
    #[cfg(feature = "bls12-381")]
    Bls12381G2(ark_bls12_381::G2Affine),
}

impl Verifier {
    /// A verifier for `chain`.
    ///
    /// # Errors
    ///
    /// [`Error::UnsupportedScheme`] for schemes this build cannot verify,
    /// [`Error::InvalidPublicKey`] for public keys that are malformed,
    /// non-canonical, the wrong length for the scheme's group, the point at
    /// infinity, or outside the prime-order subgroup, and [`Error::Length`]
    /// for a chained scheme whose genesis seed is not 32 bytes.
    pub fn new(chain: &ChainInfo) -> Result<Self, Error> {
        let key = Key::new(chain.scheme(), chain.public_key())?;
        // Round 1 of a chained scheme signs the seed; an empty one would make
        // its message equal to the unchained one.
        if chain.scheme().is_chained() && chain.genesis_seed().len() != 32 {
            return Err(Error::length(
                "genesis seed",
                32,
                chain.genesis_seed().len(),
            ));
        }
        Ok(Self {
            chain_hash: chain.hash(),
            scheme: chain.scheme().clone(),
            genesis_seed: chain.genesis_seed().to_vec(),
            key,
        })
    }

    /// The hash of the chain this verifier checks against.
    #[must_use]
    pub fn chain_hash(&self) -> ChainHash {
        self.chain_hash
    }

    /// Verifies `beacon` (for the round it claims).
    ///
    /// For unchained schemes any `previous_signature` is ignored and the
    /// result carries `None`, so no unauthenticated bytes are kept.
    ///
    /// # Errors
    ///
    /// [`Error::RoundZero`] for round 0, [`Error::InvalidPreviousSignature`]
    /// or [`Error::Length`] for a chained beacon without a valid previous
    /// signature, [`Error::Length`] or [`Error::MalformedSignature`] for a
    /// malformed signature, and [`Error::InvalidSignature`] if it does not
    /// verify.
    pub fn verify(&self, beacon: &Beacon) -> Result<VerifiedBeacon, Error> {
        if beacon.round == 0 {
            return Err(Error::RoundZero);
        }
        let previous_signature = if self.scheme.is_chained() {
            let prev = beacon
                .previous_signature
                .as_deref()
                .ok_or(Error::InvalidPreviousSignature)?;
            if beacon.round == 1 {
                // Round 1 signs the genesis seed.
                if prev != self.genesis_seed.as_slice() {
                    return Err(Error::InvalidPreviousSignature);
                }
            } else if prev.len() != self.key.signature_len() {
                return Err(Error::length(
                    "previous signature",
                    self.key.signature_len(),
                    prev.len(),
                ));
            }
            Some(prev.to_vec())
        } else {
            None
        };
        let message = match &previous_signature {
            Some(prev) => sha256(&[prev, &beacon.round.to_be_bytes()]),
            None => sha256(&[&beacon.round.to_be_bytes()]),
        };
        self.key.verify(&beacon.signature, &message)?;
        Ok(VerifiedBeacon {
            chain_hash: self.chain_hash,
            randomness: beacon.randomness(),
            beacon: Beacon {
                round: beacon.round,
                signature: beacon.signature.clone(),
                previous_signature,
            },
        })
    }

    /// Verifies `beacon` and requires it to be for `round`.
    ///
    /// # Errors
    ///
    /// [`Error::RoundMismatch`] if the beacon is for another round, and
    /// otherwise as [`Verifier::verify`].
    pub fn verify_round(&self, round: u64, beacon: &Beacon) -> Result<VerifiedBeacon, Error> {
        if round == 0 {
            return Err(Error::RoundZero);
        }
        if beacon.round != round {
            return Err(Error::RoundMismatch {
                expected: round,
                got: beacon.round,
            });
        }
        self.verify(beacon)
    }
}

#[cfg(feature = "bls12-381")]
mod bls12_381 {
    use alloc::vec::Vec;

    use ark_bls12_381::{Bls12_381, G1Affine, G1Projective, G2Affine, G2Projective, g1, g2};
    use ark_ec::{
        AffineRepr, CurveGroup,
        hashing::{HashToCurve, curve_maps::wb::WBMap, map_to_curve_hasher::MapToCurveBasedHasher},
        pairing::Pairing,
    };
    use ark_ff::{Zero, field_hashers::DefaultFieldHasher};
    use ark_serialize::{CanonicalDeserialize, CanonicalSerialize, Compress, Validate};
    use sha2::Sha256;

    /// Hash-to-curve tag for signatures on G1 (quicknet).
    pub const DST_G1: &[u8] = b"BLS_SIG_BLS12381G1_XMD:SHA-256_SSWU_RO_NUL_";
    /// Hash-to-curve tag for signatures on G2 (pedersen schemes).
    pub const DST_G2: &[u8] = b"BLS_SIG_BLS12381G2_XMD:SHA-256_SSWU_RO_NUL_";

    pub const G1_LEN: usize = 48;
    pub const G2_LEN: usize = 96;

    /// Decodes a compressed point, accepting only the one canonical encoding
    /// of a non-infinity point in the prime-order subgroup.
    pub fn decode<P: CanonicalDeserialize + CanonicalSerialize + AffineRepr>(
        bytes: &[u8],
        len: usize,
    ) -> Option<P> {
        if bytes.len() != len {
            return None; // arkworks ignores trailing bytes
        }
        let point = P::deserialize_with_mode(bytes, Compress::Yes, Validate::Yes).ok()?;
        if point.is_zero() {
            return None;
        }
        // Belt and braces: the encoding must round-trip byte for byte.
        let mut again = Vec::with_capacity(len);
        point.serialize_compressed(&mut again).ok()?;
        (again == bytes).then_some(point)
    }

    #[cfg(feature = "test-chain")]
    pub fn encode<P: CanonicalSerialize>(point: &P) -> Vec<u8> {
        let mut out = Vec::new();
        point
            .serialize_compressed(&mut out)
            .expect("serializing into a Vec");
        out
    }

    pub fn hash_to_g1(message: &[u8], dst: &[u8]) -> G1Affine {
        MapToCurveBasedHasher::<G1Projective, DefaultFieldHasher<Sha256, 128>, WBMap<g1::Config>>::new(dst)
            .expect("valid tag")
            .hash(message)
            .expect("hash to curve")
    }

    pub fn hash_to_g2(message: &[u8], dst: &[u8]) -> G2Affine {
        MapToCurveBasedHasher::<G2Projective, DefaultFieldHasher<Sha256, 128>, WBMap<g2::Config>>::new(dst)
            .expect("valid tag")
            .hash(message)
            .expect("hash to curve")
    }

    /// `e(sig, -g2) · e(H(m), pk) == 1`
    pub fn verify_sig_g1(pk: &G2Affine, sig: &G1Affine, message: &[u8]) -> bool {
        let hm = hash_to_g1(message, DST_G1);
        let neg_g2 = (-G2Affine::generator().into_group()).into_affine();
        Bls12_381::multi_pairing([*sig, hm], [neg_g2, *pk]).is_zero()
    }

    /// `e(-g1, sig) · e(pk, H(m)) == 1`
    pub fn verify_sig_g2(pk: &G1Affine, sig: &G2Affine, message: &[u8]) -> bool {
        let hm = hash_to_g2(message, DST_G2);
        let neg_g1 = (-G1Affine::generator().into_group()).into_affine();
        Bls12_381::multi_pairing([neg_g1, *pk], [*sig, hm]).is_zero()
    }
}

#[cfg(feature = "test-chain")]
pub(crate) use bls12_381::{DST_G1, DST_G2, encode, hash_to_g1, hash_to_g2};

impl Key {
    fn new(scheme: &Scheme, public_key: &[u8]) -> Result<Self, Error> {
        match scheme {
            #[cfg(feature = "bls12-381")]
            Scheme::PedersenBlsChained | Scheme::PedersenBlsUnchained => {
                if public_key.len() != bls12_381::G1_LEN {
                    return Err(Error::InvalidPublicKey);
                }
                bls12_381::decode(public_key, bls12_381::G1_LEN)
                    .map(Self::Bls12381G1)
                    .ok_or(Error::InvalidPublicKey)
            }
            #[cfg(feature = "bls12-381")]
            Scheme::BlsUnchainedG1Rfc9380 => {
                if public_key.len() != bls12_381::G2_LEN {
                    return Err(Error::InvalidPublicKey);
                }
                bls12_381::decode(public_key, bls12_381::G2_LEN)
                    .map(Self::Bls12381G2)
                    .ok_or(Error::InvalidPublicKey)
            }
            other => {
                let _ = public_key;
                Err(Error::UnsupportedScheme(other.clone()))
            }
        }
    }

    /// Length of a signature under this key.
    fn signature_len(&self) -> usize {
        match *self {
            #[cfg(feature = "bls12-381")]
            Self::Bls12381G1(_) => bls12_381::G2_LEN,
            #[cfg(feature = "bls12-381")]
            Self::Bls12381G2(_) => bls12_381::G1_LEN,
        }
    }

    #[cfg_attr(not(feature = "bls12-381"), allow(unreachable_code, unused_variables))]
    fn verify(&self, signature: &[u8], message: &[u8]) -> Result<(), Error> {
        let len = self.signature_len();
        if signature.len() != len {
            return Err(Error::length("signature", len, signature.len()));
        }
        let ok = match *self {
            #[cfg(feature = "bls12-381")]
            Self::Bls12381G1(ref pk) => {
                let sig = bls12_381::decode(signature, len).ok_or(Error::MalformedSignature)?;
                bls12_381::verify_sig_g2(pk, &sig, message)
            }
            #[cfg(feature = "bls12-381")]
            Self::Bls12381G2(ref pk) => {
                let sig = bls12_381::decode(signature, len).ok_or(Error::MalformedSignature)?;
                bls12_381::verify_sig_g1(pk, &sig, message)
            }
        };
        if ok {
            Ok(())
        } else {
            Err(Error::InvalidSignature)
        }
    }
}
