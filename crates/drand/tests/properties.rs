//! Property tests: schedule arithmetic, `TestChain` sign → verify, tampering, serde.

#![cfg(not(target_arch = "wasm32"))]

use std::num::NonZeroU32;

use drand::{Beacon, Schedule, UnixTime};
#[cfg(feature = "bls12-381")]
use drand::{Verifier, chains};
use proptest::prelude::*;

#[cfg(feature = "test-chain")]
use drand::{Scheme, TestChain};

proptest! {
    #[test]
    fn schedule_round_trips(genesis in -1_000_000_000i64..4_000_000_000, period in 1u32..100_000, round in 1u64..10_000_000_000) {
        let s = Schedule::new(genesis, NonZeroU32::new(period).unwrap());
        let t = s.round_time(round).unwrap();
        let ms = |d: i64| UnixTime::from_millis(t.as_millis() + d);
        prop_assert_eq!(s.round_at(t), Some(round));
        prop_assert_eq!(s.round_at(ms(i64::from(period) * 1000 - 1)), Some(round));
        prop_assert_eq!(s.round_after(t), round + 1);
        prop_assert!(s.round_time(s.round_after(t)).unwrap() > t);
        prop_assert_eq!(s.round_at(ms(-1)), round.checked_sub(1).filter(|r| *r > 0));
    }

    #[test]
    fn round_after_is_strictly_future(genesis in 0i64..2_000_000_000, period in 1u32..120, t in -10_000_000i64..4_000_000_000_000) {
        let s = Schedule::new(genesis, NonZeroU32::new(period).unwrap());
        let t = UnixTime::from_millis(t);
        let r = s.round_after(t);
        prop_assert!(s.round_time(r).unwrap() > t);
        if let Some(prev) = r.checked_sub(1).filter(|p| *p > 0) {
            prop_assert!(s.round_time(prev).unwrap() <= t);
        }
    }

    #[test]
    fn beacon_serde_round_trips(round in 1u64.., sig in proptest::collection::vec(any::<u8>(), 0..100), prev in proptest::option::of(proptest::collection::vec(any::<u8>(), 1..100))) {
        let b = Beacon { round, signature: sig, previous_signature: prev };
        let json = serde_json::to_string(&b).unwrap();
        prop_assert_eq!(&serde_json::from_str::<Beacon>(&json).unwrap(), &b);
        let bin = postcard::to_allocvec(&b).unwrap();
        prop_assert_eq!(&postcard::from_bytes::<Beacon>(&bin).unwrap(), &b);
    }
}

#[cfg(feature = "bls12-381")]
#[test]
fn v1_shape_with_randomness_deserializes_and_verifies() {
    let old = r#"{"round":1000,"signature":"b44679b9a59af2ec876b1a6b1ad52ea9b1615fc3982b19576350f93447cb1125e342b73a8dd2bacbe47e4b6b63ed5e39","randomness":"fe290beca10872ef2fb164d2aa4442de4566183ec51c56ff3cd603d930e54fdd"}"#;
    let b: Beacon = serde_json::from_str(old).unwrap();
    Verifier::new(&chains::quicknet())
        .unwrap()
        .verify_round(1000, &b)
        .unwrap();
}

#[cfg(feature = "bls12-381")]
#[test]
fn verified_beacon_serializes_with_chain_hash() {
    let b: Beacon = serde_json::from_str(r#"{"round":1000,"signature":"b44679b9a59af2ec876b1a6b1ad52ea9b1615fc3982b19576350f93447cb1125e342b73a8dd2bacbe47e4b6b63ed5e39"}"#).unwrap();
    let v = Verifier::new(&chains::quicknet())
        .unwrap()
        .verify(&b)
        .unwrap();
    let json: serde_json::Value = serde_json::to_value(&v).unwrap();
    assert_eq!(json["chain_hash"], chains::QUICKNET_HASH.to_string());
    assert_eq!(json["round"], 1000);
    assert!(postcard::to_allocvec(&v).is_ok());
}

#[cfg(feature = "test-chain")]
mod test_chain {
    use super::*;

    fn chain(scheme: Scheme) -> TestChain {
        TestChain::new(
            [7; 32],
            scheme,
            NonZeroU32::new(3).unwrap(),
            1_700_000_000,
            "test",
        )
        .unwrap()
    }

    #[test]
    fn every_supported_scheme_signs_verifiable_beacons() {
        for scheme in [
            Scheme::BlsUnchainedG1Rfc9380,
            Scheme::PedersenBlsUnchained,
            Scheme::PedersenBlsChained,
        ] {
            let tc = chain(scheme.clone());
            let info = tc.chain();
            assert_eq!(info.scheme(), &scheme);
            assert!(chains::by_hash(&info.hash()).is_none());
            let verifier = Verifier::new(info).unwrap();
            for round in 1..=4 {
                let b = tc.sign(round);
                let v = verifier.verify_round(round, &b).unwrap();
                assert_eq!(v.beacon(), &b);
                // Tampering always fails.
                let mut bad = b.clone();
                bad.signature[5] ^= 0x10;
                assert!(verifier.verify(&bad).is_err());
            }
            // Deterministic.
            assert_eq!(tc.sign(3), chain(scheme).sign(3));
        }
    }

    #[test]
    fn chained_test_chain_links_rounds() {
        let tc = chain(Scheme::PedersenBlsChained);
        let r1 = tc.sign(1);
        assert_eq!(
            r1.previous_signature.as_deref(),
            Some(tc.chain().genesis_seed())
        );
        assert_eq!(tc.sign(2).previous_signature, Some(r1.signature));
    }

    #[test]
    fn different_seeds_are_different_chains() {
        let a = chain(Scheme::BlsUnchainedG1Rfc9380);
        let b = TestChain::new(
            [8; 32],
            Scheme::BlsUnchainedG1Rfc9380,
            NonZeroU32::new(3).unwrap(),
            1_700_000_000,
            "test",
        )
        .unwrap();
        assert_ne!(a.chain().hash(), b.chain().hash());
        let verifier = Verifier::new(a.chain()).unwrap();
        assert!(verifier.verify(&b.sign(1)).is_err());
    }

    #[test]
    fn unsupported_schemes_are_refused() {
        assert!(
            TestChain::new(
                [1; 32],
                Scheme::BlsBn254UnchainedOnG1,
                NonZeroU32::new(3).unwrap(),
                0,
                "x"
            )
            .is_err()
        );
    }
}
