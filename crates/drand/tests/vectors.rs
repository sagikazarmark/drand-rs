//! Verification against real beacons (tests/data, fetched from the live relays
//! on 2026-10-07), plus negative vectors for every check in the verifier.

#![cfg(feature = "bls12-381")]

use std::{fs, num::NonZeroU32, path::Path};

use ark_bls12_381::{Fq, G2Affine};
use ark_ff::{BigInteger, PrimeField};
use ark_serialize::CanonicalSerialize;
use drand::{Beacon, ChainHash, ChainInfo, Error, Scheme, Verifier, chains};

/// The period as `ChainInfo::new` takes it (whole seconds).
fn period(chain: &ChainInfo) -> NonZeroU32 {
    NonZeroU32::new(u32::try_from(chain.period().as_secs()).unwrap()).unwrap()
}

fn data(name: &str) -> Vec<u8> {
    fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/data")
            .join(name),
    )
    .unwrap()
}

fn beacon(name: &str) -> Beacon {
    serde_json::from_slice(&data(name)).unwrap()
}

fn v1_randomness(name: &str) -> String {
    let v: serde_json::Value = serde_json::from_slice(&data(name)).unwrap();
    v["randomness"].as_str().unwrap().to_owned()
}

#[test]
fn info_parses_in_both_shapes_and_matches_built_ins() {
    for (name, built_in) in [
        ("quicknet", chains::quicknet()),
        ("default", chains::default()),
        ("evmnet", chains::evmnet()),
    ] {
        for v in ["v1", "v2"] {
            let body = data(&format!("info-{name}-{v}.json"));
            let info = ChainInfo::from_json(&body, &built_in.hash()).unwrap();
            assert_eq!(info, built_in, "{name} {v}");
        }
    }
}

#[test]
fn info_must_match_the_expected_hash() {
    let body = data("info-quicknet-v2.json");
    assert!(matches!(
        ChainInfo::from_json(&body, &chains::DEFAULT_HASH),
        Err(Error::ChainHashMismatch { .. })
    ));
    // Tampering with any hashed field changes the hash.
    let tampered = String::from_utf8(body)
        .unwrap()
        .replace("\"period\":3", "\"period\":4");
    assert!(ChainInfo::from_json(tampered.as_bytes(), &chains::QUICKNET_HASH).is_err());
}

#[test]
fn quicknet_beacons_verify() {
    let verifier = Verifier::new(&chains::quicknet()).unwrap();
    for round in [1u64, 1000, 1005, 1010, 1012, 1020, 30_000_000] {
        let name = format!("quicknet-{round}-v1.json");
        let v = verifier.verify_round(round, &beacon(&name)).unwrap();
        assert_eq!(hex::encode(v.randomness()), v1_randomness(&name));
        assert_eq!(v.chain_hash(), chains::QUICKNET_HASH);
        assert_eq!(v.beacon().previous_signature, None);
    }
    for round in [1000u64, 1005, 1010, 1012, 1020] {
        let v1 = beacon(&format!("quicknet-{round}-v1.json"));
        let v2 = beacon(&format!("quicknet-{round}-v2.json"));
        assert_eq!(v1, v2);
    }
}

#[test]
fn default_beacons_verify() {
    let verifier = Verifier::new(&chains::default()).unwrap();
    for round in [1u64, 2, 1_000_000] {
        for v in ["v1", "v2"] {
            let b = beacon(&format!("default-{round}-{v}.json"));
            let checked = verifier.verify_round(round, &b).unwrap();
            assert_eq!(
                hex::encode(checked.randomness()),
                v1_randomness(&format!("default-{round}-v1.json"))
            );
            assert_eq!(checked.beacon().previous_signature, b.previous_signature);
        }
    }
}

#[test]
fn pedersen_unchained_testnet_beacon_verifies() {
    let v: serde_json::Value = serde_json::from_slice(&data("testnet-unchained.json")).unwrap();
    let hash: ChainHash = v["info"]["hash"].as_str().unwrap().parse().unwrap();
    let info = ChainInfo::from_json(v["info"].to_string().as_bytes(), &hash).unwrap();
    assert_eq!(info.scheme(), &Scheme::PedersenBlsUnchained);
    let b: Beacon = serde_json::from_value(v["beacon"].clone()).unwrap();
    let verified = Verifier::new(&info)
        .unwrap()
        .verify_round(1_000_000, &b)
        .unwrap();
    assert_eq!(
        hex::encode(verified.randomness()),
        v["beacon"]["randomness"].as_str().unwrap()
    );
}

#[test]
fn evmnet_parses_but_is_not_verifiable_yet() {
    for v in ["v1", "v2"] {
        let b = beacon(&format!("evmnet-1000-{v}.json"));
        assert_eq!(b.signature.len(), 64);
    }
    assert!(matches!(
        Verifier::new(&chains::evmnet()),
        Err(Error::UnsupportedScheme(Scheme::BlsBn254UnchainedOnG1))
    ));
}

#[test]
fn deprecated_and_unknown_schemes_are_unsupported() {
    let q = chains::quicknet();
    for scheme in [Scheme::BlsUnchainedOnG1, Scheme::parse("future-scheme")] {
        let info = ChainInfo::new(
            q.public_key().to_vec(),
            period(&q),
            q.genesis_time().as_secs(),
            q.genesis_seed().to_vec(),
            scheme,
            q.beacon_id(),
        );
        assert!(matches!(
            Verifier::new(&info),
            Err(Error::UnsupportedScheme(_))
        ));
    }
}

// ---------------------------------------------------------------------------
// Negative vectors
// ---------------------------------------------------------------------------

fn quicknet_1000() -> (Verifier, Beacon) {
    (
        Verifier::new(&chains::quicknet()).unwrap(),
        beacon("quicknet-1000-v1.json"),
    )
}

#[test]
fn tampered_signature_or_round_fails() {
    let (verifier, b) = quicknet_1000();
    for i in [0usize, 10, 47] {
        let mut bad = b.clone();
        bad.signature[i] ^= 0x01;
        assert!(matches!(
            verifier.verify(&bad),
            Err(Error::InvalidSignature | Error::MalformedSignature)
        ));
    }
    let mut other_round = b.clone();
    other_round.round = 1001;
    assert_eq!(verifier.verify(&other_round), Err(Error::InvalidSignature));
    assert!(matches!(
        verifier.verify_round(1001, &b),
        Err(Error::RoundMismatch { .. })
    ));
    let mut zero = b.clone();
    zero.round = 0;
    assert_eq!(verifier.verify(&zero), Err(Error::RoundZero));
    assert_eq!(verifier.verify_round(0, &b), Err(Error::RoundZero));
}

#[test]
fn trailing_bytes_are_rejected() {
    let (verifier, mut b) = quicknet_1000();
    b.signature.extend_from_slice(&[0, 0]);
    assert!(matches!(verifier.verify(&b), Err(Error::Length { .. })));
}

/// Adds the field modulus to the x coordinate of a compressed point,
/// keeping the flag bits: a second encoding of the same x, if it fits.
fn x_plus_p(compressed: &[u8]) -> Option<Vec<u8>> {
    let flags = compressed[0] & 0xe0;
    let mut x = compressed.to_vec();
    x[0] &= 0x1f;
    let p = Fq::MODULUS.to_bytes_be();
    let mut out = vec![0u8; x.len()];
    let mut carry = 0u16;
    for i in (0..x.len()).rev() {
        let pi = p.len().checked_sub(x.len() - i).map_or(0, |j| p[j]);
        let s = u16::from(x[i]) + u16::from(pi) + carry;
        let [low, high] = s.to_le_bytes();
        out[i] = low;
        carry = u16::from(high);
    }
    (carry == 0 && out[0] & 0xe0 == 0).then(|| {
        out[0] |= flags;
        out
    })
}

#[test]
fn non_canonical_x_is_rejected() {
    // Find a published signature whose x + p still fits in 381 bits.
    let verifier = Verifier::new(&chains::quicknet()).unwrap();
    let mut tried = 0;
    for round in [1u64, 1000, 1005, 1010, 1012, 1020, 30_000_000] {
        let b = beacon(&format!("quicknet-{round}-v1.json"));
        if let Some(sig) = x_plus_p(&b.signature) {
            tried += 1;
            let bad = Beacon {
                signature: sig,
                ..b
            };
            assert_eq!(verifier.verify(&bad), Err(Error::MalformedSignature));
        }
    }
    assert!(tried > 0, "no fixture allowed an x + p encoding");
}

#[test]
fn infinity_is_rejected() {
    let (verifier, b) = quicknet_1000();
    let mut inf = vec![0u8; 48];
    inf[0] = 0xc0; // compressed + infinity
    assert_eq!(
        verifier.verify(&Beacon {
            signature: inf,
            ..b
        }),
        Err(Error::MalformedSignature)
    );
    // A G2 infinity "key" is rejected too.
    let q = chains::quicknet();
    let mut key = vec![0u8; 96];
    key[0] = 0xc0;
    let info = ChainInfo::new(
        key,
        period(&q),
        q.genesis_time().as_secs(),
        q.genesis_seed().to_vec(),
        Scheme::BlsUnchainedG1Rfc9380,
        "x",
    );
    assert!(matches!(Verifier::new(&info), Err(Error::InvalidPublicKey)));
}

#[test]
fn non_subgroup_g2_signature_is_rejected() {
    // An on-curve G2 point outside the prime-order subgroup.
    let mut x = 1u64;
    let point = loop {
        let fx = ark_bls12_381::Fq2::new(Fq::from(x), Fq::from(0u64));
        if let Some(p) = G2Affine::get_point_from_x_unchecked(fx, false)
            && !p.is_in_correct_subgroup_assuming_on_curve()
        {
            break p;
        }
        x += 1;
    };
    let mut sig = Vec::new();
    point.serialize_compressed(&mut sig).unwrap();
    let verifier = Verifier::new(&chains::default()).unwrap();
    let mut b = beacon("default-2-v1.json");
    b.signature = sig;
    assert_eq!(verifier.verify(&b), Err(Error::MalformedSignature));
}

#[test]
fn chained_previous_signature_rules() {
    let verifier = Verifier::new(&chains::default()).unwrap();
    let r1 = beacon("default-1-v1.json");
    let r2 = beacon("default-2-v1.json");

    let mut missing = r2.clone();
    missing.previous_signature = None;
    assert_eq!(
        verifier.verify(&missing),
        Err(Error::InvalidPreviousSignature)
    );

    let mut short = r2.clone();
    short.previous_signature = Some(vec![1; 32]);
    assert!(matches!(verifier.verify(&short), Err(Error::Length { .. })));

    let mut wrong = r2.clone();
    wrong.previous_signature = Some(r1.signature.iter().map(|b| b ^ 1).collect());
    assert_eq!(verifier.verify(&wrong), Err(Error::InvalidSignature));

    let mut not_seed = r1.clone();
    not_seed.previous_signature = Some(vec![0; 32]);
    assert_eq!(
        verifier.verify(&not_seed),
        Err(Error::InvalidPreviousSignature)
    );
    let mut empty = r1.clone();
    empty.previous_signature = Some(vec![]);
    assert_eq!(
        verifier.verify(&empty),
        Err(Error::InvalidPreviousSignature)
    );
}

#[test]
fn unchained_drops_unauthenticated_previous_signature() {
    let (verifier, mut b) = quicknet_1000();
    b.previous_signature = Some(vec![0xde, 0xad]);
    let v = verifier.verify(&b).unwrap();
    assert_eq!(v.beacon().previous_signature, None);
}

#[test]
fn mislabeled_schemes_never_verify() {
    // The default chain's key labelled as unchained: real chained signatures fail.
    let d = chains::default();
    let relabeled = ChainInfo::new(
        d.public_key().to_vec(),
        period(&d),
        d.genesis_time().as_secs(),
        d.genesis_seed().to_vec(),
        Scheme::PedersenBlsUnchained,
        d.beacon_id(),
    );
    assert_eq!(relabeled.hash(), d.hash(), "the scheme is not hashed");
    let verifier = Verifier::new(&relabeled).unwrap();
    assert_eq!(
        verifier.verify(&beacon("default-2-v1.json")),
        Err(Error::InvalidSignature)
    );
    // quicknet's G2 key labelled as a G1-key scheme: rejected up front.
    let q = chains::quicknet();
    let wrong_group = ChainInfo::new(
        q.public_key().to_vec(),
        period(&q),
        q.genesis_time().as_secs(),
        q.genesis_seed().to_vec(),
        Scheme::PedersenBlsUnchained,
        q.beacon_id(),
    );
    assert_eq!(
        Verifier::new(&wrong_group).unwrap_err(),
        Error::InvalidPublicKey
    );
    // A quicknet beacon against the default chain: wrong signature group.
    let verifier = Verifier::new(&chains::default()).unwrap();
    let mut b = beacon("quicknet-1000-v1.json");
    b.previous_signature = Some(beacon("default-2-v1.json").signature);
    assert!(matches!(verifier.verify(&b), Err(Error::Length { .. })));
}

#[test]
fn period_must_be_non_zero() {
    let body = String::from_utf8(data("info-quicknet-v2.json"))
        .unwrap()
        .replace("\"period\":3", "\"period\":0");
    assert!(matches!(
        ChainInfo::from_json(body.as_bytes(), &chains::QUICKNET_HASH),
        Err(Error::Parse { what: "period", .. })
    ));
    assert!(NonZeroU32::new(0).is_none());
}

#[test]
fn info_fields_cannot_be_resplit() {
    // drand's chain hash concatenates key ‖ seed ‖ beacon ID without lengths.
    let body = |seed: &str, id: &str, scheme: &str, key: &str| {
        format!(
            r#"{{"public_key":"{key}","period":30,"genesis_time":1595431050,"genesis_seed":"{seed}","scheme":"{scheme}","beacon_id":"{id}"}}"#
        )
    };
    let d = chains::default();
    let key = hex::encode(d.public_key());
    let seed = hex::encode(d.genesis_seed());
    // Last seed byte (0x0a) moved into the beacon ID: same hash, different chain.
    let resplit = body(&seed[..62], "\\n", "pedersen-bls-chained", &key);
    assert!(matches!(
        ChainInfo::from_json(resplit.as_bytes(), &chains::DEFAULT_HASH),
        Err(Error::Length { .. })
    ));
    // The real thing still parses.
    let real = body(&seed, "default", "pedersen-bls-chained", &key);
    assert_eq!(
        ChainInfo::from_json(real.as_bytes(), &chains::DEFAULT_HASH).unwrap(),
        d
    );
    // A built-in chain relabeled with another scheme is refused too.
    let relabeled = body(&seed, "default", "pedersen-bls-unchained", &key);
    assert!(ChainInfo::from_json(relabeled.as_bytes(), &chains::DEFAULT_HASH).is_err());
}

#[test]
fn chained_chains_need_a_32_byte_seed() {
    let d = chains::default();
    let empty = ChainInfo::new(
        d.public_key().to_vec(),
        period(&d),
        d.genesis_time().as_secs(),
        vec![],
        Scheme::PedersenBlsChained,
        "x",
    );
    assert!(matches!(Verifier::new(&empty), Err(Error::Length { .. })));
}

#[test]
fn flag_variants_and_non_subgroup_g1_are_rejected() {
    let (verifier, b) = quicknet_1000();
    for flags in 0u8..8 {
        let mut sig = b.signature.clone();
        let original = sig[0] >> 5;
        if flags == original {
            continue;
        }
        sig[0] = (sig[0] & 0x1f) | (flags << 5);
        assert!(
            verifier
                .verify(&Beacon {
                    signature: sig,
                    ..b.clone()
                })
                .is_err(),
            "flags {flags:03b}"
        );
    }
    // An on-curve G1 point outside the prime-order subgroup.
    let mut x = 1u64;
    let point = loop {
        if let Some(p) = ark_bls12_381::G1Affine::get_point_from_x_unchecked(Fq::from(x), false)
            && !p.is_in_correct_subgroup_assuming_on_curve()
        {
            break p;
        }
        x += 1;
    };
    let mut sig = Vec::new();
    point.serialize_compressed(&mut sig).unwrap();
    assert_eq!(
        verifier.verify(&Beacon {
            signature: sig,
            ..b
        }),
        Err(Error::MalformedSignature)
    );
}
