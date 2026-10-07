use alloc::{string::String, vec::Vec};
use core::{fmt, num::NonZeroU32};

use ark_bls12_381::{Fr, G1Affine, G2Affine};
use ark_ec::{AffineRepr, CurveGroup};
use ark_ff::{PrimeField, Zero};

use crate::{
    Beacon, ChainInfo, Error, Scheme, sha256,
    verify::{DST_G1, DST_G2, encode, hash_to_g1, hash_to_g2},
};

/// A deterministic drand-like chain that signs its own beacons.
///
/// The secret key is derived from `seed`, so **anyone with the seed can forge
/// its beacons**: use it for tests and demos only. Its chain hash differs from
/// every real chain, so its beacons never verify as real drand randomness.
///
/// Supports the BLS12-381 schemes: [`Scheme::BlsUnchainedG1Rfc9380`],
/// [`Scheme::PedersenBlsUnchained`] and [`Scheme::PedersenBlsChained`].
#[derive(Clone)]
pub struct TestChain {
    secret: Fr,
    info: ChainInfo,
}

impl fmt::Debug for TestChain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TestChain")
            .field("info", &self.info)
            .finish_non_exhaustive()
    }
}

impl TestChain {
    /// A test chain.
    ///
    /// # Errors
    ///
    /// [`Error::UnsupportedScheme`] for schemes it cannot sign.
    ///
    /// # Panics
    ///
    /// If `seed` derives a zero secret key (with negligible probability).
    pub fn new(
        seed: [u8; 32],
        scheme: Scheme,
        period: NonZeroU32,
        genesis_time: i64,
        beacon_id: impl Into<String>,
    ) -> Result<Self, Error> {
        let secret =
            Fr::from_be_bytes_mod_order(&sha256(&[b"drand-rs/test-chain/v1/secret-key", &seed]));
        assert!(!secret.is_zero(), "degenerate test chain seed");
        let public_key = match scheme {
            Scheme::BlsUnchainedG1Rfc9380 => {
                encode(&(G2Affine::generator() * secret).into_affine())
            }
            Scheme::PedersenBlsChained | Scheme::PedersenBlsUnchained => {
                encode(&(G1Affine::generator() * secret).into_affine())
            }
            other => return Err(Error::UnsupportedScheme(other)),
        };
        let genesis_seed = sha256(&[b"drand-rs/test-chain/v1/genesis-seed", &seed]).to_vec();
        let info = ChainInfo::new(
            public_key,
            period,
            genesis_time,
            genesis_seed,
            scheme,
            beacon_id,
        );
        Ok(Self { secret, info })
    }

    /// The chain's parameters.
    #[must_use]
    pub fn chain(&self) -> &ChainInfo {
        &self.info
    }

    fn signature(&self, message: &[u8]) -> Vec<u8> {
        match self.info.scheme() {
            Scheme::BlsUnchainedG1Rfc9380 => {
                encode(&(hash_to_g1(message, DST_G1) * self.secret).into_affine())
            }
            _ => encode(&(hash_to_g2(message, DST_G2) * self.secret).into_affine()),
        }
    }

    /// Signs `round` (≥ 1).
    ///
    /// Pure and stateless. Unchained schemes take O(1); the chained scheme
    /// re-signs every round from 1, so keep `genesis_time` close to now.
    ///
    /// # Panics
    ///
    /// If `round` is 0.
    #[must_use]
    pub fn sign(&self, round: u64) -> Beacon {
        assert!(round > 0, "round 0 does not exist");
        if !self.info.scheme().is_chained() {
            return Beacon {
                round,
                signature: self.signature(&sha256(&[&round.to_be_bytes()])),
                previous_signature: None,
            };
        }
        let mut prev = self.info.genesis_seed().to_vec();
        let mut signature = Vec::new();
        for r in 1..=round {
            if r > 1 {
                prev = core::mem::take(&mut signature);
            }
            signature = self.signature(&sha256(&[&prev, &r.to_be_bytes()]));
        }
        Beacon {
            round,
            signature,
            previous_signature: Some(prev),
        }
    }
}

const _: () = {
    const fn send_sync<T: Send + Sync>() {}
    send_sync::<TestChain>();
};
