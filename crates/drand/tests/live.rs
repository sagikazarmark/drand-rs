//! Live tests against the public drand relays. Ignored by default:
//!
//! ```sh
//! cargo test -p drand --features client --test live -- --ignored
//! ```
//!
//! Every check runs against each relay and API version on its own (no hedging
//! across relays), so one broken relay cannot hide behind another, and all
//! failing relays are reported together.

#![cfg(all(
    feature = "client",
    any(feature = "default-tls", feature = "rustls", feature = "native-tls"),
    not(target_arch = "wasm32")
))]

use std::{future::Future, time::Duration};

use drand::{
    ChainInfo, UnixTime, chains,
    client::{Client, Fetch},
    http::{Relay, relays},
};

/// Every public relay, with every API version it serves.
fn relays() -> Vec<Relay> {
    vec![
        Relay::v2(relays::API_DRAND_SH).unwrap(),
        Relay::v2(relays::API2_DRAND_SH).unwrap(),
        Relay::v2(relays::API3_DRAND_SH).unwrap(),
        Relay::v1(relays::API_DRAND_SH).unwrap(),
        Relay::v1(relays::API2_DRAND_SH).unwrap(),
        Relay::v1(relays::API3_DRAND_SH).unwrap(),
        Relay::v1(relays::CLOUDFLARE).unwrap(),
    ]
}

fn client(chain: ChainInfo, relay: &Relay) -> Client {
    Client::builder(chain)
        .relays(vec![relay.clone()])
        .request_timeout(Duration::from_secs(10))
        .build()
        .unwrap()
}

/// Runs `check` against every relay and fails with all failing relays at once.
async fn for_each_relay<F, Fut>(what: &str, check: F)
where
    F: Fn(Relay) -> Fut,
    Fut: Future<Output = Result<(), String>>,
{
    let mut failures = Vec::new();
    for relay in relays() {
        if let Err(e) = check(relay.clone()).await {
            failures.push(format!("{relay}: {e}"));
        }
    }
    assert!(
        failures.is_empty(),
        "{what} failed on {} relay(s):\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[tokio::test]
#[ignore = "talks to the public drand relays"]
async fn info_matches_the_pinned_chains() {
    let http = reqwest::Client::new();
    for_each_relay("chain info", |relay| {
        let http = http.clone();
        async move {
            for chain in [chains::quicknet(), chains::default()] {
                let hash = chain.hash();
                let url = relay.info_url(&hash);
                let body = http
                    .get(&url)
                    .timeout(Duration::from_secs(10))
                    .send()
                    .await
                    .and_then(reqwest::Response::error_for_status)
                    .map_err(|e| format!("{url}: {e}"))?
                    .bytes()
                    .await
                    .map_err(|e| format!("{url}: {e}"))?;
                let info = ChainInfo::from_json(&body, &hash).map_err(|e| format!("{url}: {e}"))?;
                if info != chain {
                    return Err(format!("{url}: differs from the built-in chain"));
                }
            }
            Ok(())
        }
    })
    .await;
}

#[tokio::test]
#[ignore = "talks to the public drand relays"]
async fn quicknet_round_verifies_on_every_relay() {
    for_each_relay("quicknet round 1000", |relay| async move {
        match client(chains::quicknet(), &relay).fetch(1000).await {
            Ok(Fetch::Ready(b)) => {
                let r = hex::encode(b.randomness());
                if r == "fe290beca10872ef2fb164d2aa4442de4566183ec51c56ff3cd603d930e54fdd" {
                    Ok(())
                } else {
                    Err(format!("unexpected randomness {r}"))
                }
            }
            other => Err(format!("{other:?}")),
        }
    })
    .await;
}

#[tokio::test]
#[ignore = "talks to the public drand relays"]
async fn default_rounds_verify_on_every_relay() {
    // Round 1 signs the genesis seed; round 2 signs round 1's signature.
    let known = [
        (
            1,
            "101297f1ca7dc44ef6088d94ad5fb7ba03455dc33d53ddb412bbc4564ed986ec",
        ),
        (
            2,
            "e8fee7dac6eb2b89df97d631cfccedbada7d5d05495bb546eef462e4145fdf8f",
        ),
    ];
    for_each_relay("default chain rounds 1 and 2", |relay| async move {
        let c = client(chains::default(), &relay);
        for (round, randomness) in known {
            match c.fetch(round).await {
                Ok(Fetch::Ready(b)) if hex::encode(b.randomness()) == randomness => {}
                other => return Err(format!("round {round}: {other:?}")),
            }
        }
        Ok(())
    })
    .await;
}

#[tokio::test]
#[ignore = "talks to the public drand relays"]
async fn future_rounds_are_not_yet_on_every_relay() {
    let quicknet = chains::quicknet();
    let schedule = quicknet.schedule();
    let now = UnixTime::from(std::time::SystemTime::now());
    // Ten minutes ahead: never published while the test runs.
    let round = schedule.round_at(now).unwrap() + 200;
    let http = reqwest::Client::new();
    for_each_relay("future round", |relay| {
        let (quicknet, http) = (quicknet.clone(), http.clone());
        async move {
            // The raw answer: 425 (drand.sh) or 404 (Cloudflare).
            let url = relay.round_url(
                &quicknet.hash(),
                drand::http::RoundRef::Number(round.try_into().unwrap()),
            );
            let status = http
                .get(&url)
                .timeout(Duration::from_secs(10))
                .send()
                .await
                .map_err(|e| format!("{url}: {e}"))?
                .status()
                .as_u16();
            if status != 425 && status != 404 {
                return Err(format!("{url}: HTTP {status}, expected 425 or 404"));
            }
            match client(quicknet.clone(), &relay).fetch(round).await {
                Ok(Fetch::NotYet { due_at }) if Some(due_at) == schedule.round_time(round) => {
                    Ok(())
                }
                other => Err(format!("{other:?}")),
            }
        }
    })
    .await;
}

#[tokio::test]
#[ignore = "talks to the public drand relays"]
async fn latest_and_the_next_round_with_hedging() {
    let c = Client::builder(chains::quicknet()).build().unwrap();
    let latest = c.latest().await.unwrap();
    assert!(latest.round() > 30_000_000);
    assert_eq!(latest.chain_hash(), chains::QUICKNET_HASH);
    let next = c
        .wait_for(latest.round() + 1, Duration::from_secs(15))
        .await
        .unwrap();
    assert_eq!(next.round(), latest.round() + 1);
}

#[test]
fn every_default_relay_is_covered() {
    let all = relays();
    for chain in [
        chains::QUICKNET_HASH,
        chains::DEFAULT_HASH,
        chains::EVMNET_HASH,
    ] {
        for relay in relays::defaults(&chain) {
            assert!(all.contains(&relay), "{relay} is not live-tested");
        }
    }
}
