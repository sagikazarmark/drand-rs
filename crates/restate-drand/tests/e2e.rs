//! `Drand` in a real `restate-server`: commit and await, the durable sleep,
//! the fetch budget, and cancellation.
//!
//! Ignored: run with `RESTATE_SERVER_BIN` set and `-- --ignored`.

#![cfg(unix)]
#![expect(
    missing_docs,
    reason = "`#[restate_sdk::service]` generates undocumented public items"
)]

use std::{
    num::NonZeroU32,
    time::{Duration, Instant},
};

use drand::{ChainInfo, Scheme, TestChain, Verifier, chains, client::Client, http::Relay};
use restate_drand::{AwaitError, CLOCK_STEP, Drand, FETCH_STEP};
use restate_e2e_harness::{Call, ReusePolicy, ServerSpec, launcher_or_skip};
use restate_sdk::{endpoint::Endpoint, prelude::*};
use serde_json::{Value, json};

const SERVER: ServerSpec = ServerSpec {
    name: "restate-drand",
    features: &[],
    env: &[],
};

/// A minimal user: commit, then await. "Unavailable" is an answer, not a failure.
struct Lottery {
    drand: Drand,
}

#[restate_sdk::service(name = "Lottery")]
impl Lottery {
    #[handler]
    async fn draw(&self, ctx: Context<'_>) -> HandlerResult<Json<Value>> {
        let commitment = self.drand.commit_round(&ctx).await?;
        self.answer(&ctx, commitment.round).await
    }

    #[handler(name = "awaitRound")]
    async fn await_round(&self, ctx: Context<'_>, round: Json<u64>) -> HandlerResult<Json<Value>> {
        self.answer(&ctx, round.into_inner()).await
    }
}

impl Lottery {
    async fn answer(&self, ctx: &Context<'_>, round: u64) -> HandlerResult<Json<Value>> {
        match self.drand.await_beacon(ctx, round).await {
            Ok(b) => Ok(Json(json!({
                "round": b.round(),
                "randomness": hex::encode(b.randomness()),
            }))),
            Err(AwaitError::Unavailable { round, reason }) => {
                Ok(Json(json!({ "unavailable": round, "reason": reason })))
            }
            Err(e) => Err(e.into_handler_error()),
        }
    }
}

fn test_chain() -> TestChain {
    // quicknet's timing, so round numbers are current.
    let q = chains::quicknet();
    TestChain::new(
        [1; 32],
        Scheme::BlsUnchainedG1Rfc9380,
        period(&q),
        q.genesis_time().as_secs(),
        "restate-drand-test",
    )
    .unwrap()
}

/// The period as `TestChain::new` takes it (whole seconds).
fn period(chain: &ChainInfo) -> NonZeroU32 {
    NonZeroU32::new(u32::try_from(chain.period().as_secs()).unwrap()).unwrap()
}

fn mock(lead: Duration) -> Drand {
    let tc = test_chain();
    let client = Client::builder(tc.chain().clone())
        .test_transport(tc)
        .build()
        .unwrap();
    Drand::new(client)
        .with_lead(lead)
        .with_fetch_budget(Duration::from_secs(5))
}

async fn launch(drand: Drand) -> restate_e2e_harness::Restate {
    let launcher = launcher_or_skip(ReusePolicy::Never).expect("RESTATE_SERVER_BIN");
    let restate = launcher.launch(&SERVER).await;
    restate
        .deploy(Endpoint::builder().bind(Lottery { drand }).build())
        .await;
    restate
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs RESTATE_SERVER_BIN"]
async fn commits_and_awaits_a_verified_beacon() {
    if launcher_or_skip(ReusePolicy::Never).is_none() {
        return;
    }
    let restate = launch(mock(Duration::ZERO)).await;
    let reply = restate
        .invoke(&Call::service("Lottery", "draw"), None, None)
        .await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    let round = reply.body["round"].as_u64().unwrap();
    assert_eq!(
        reply.body["randomness"],
        hex::encode(
            Verifier::new(test_chain().chain())
                .unwrap()
                .verify(&test_chain().sign(round))
                .unwrap()
                .randomness()
        )
    );
    // Lead 0: the latest published round, no sleep.
    let runs = restate.admin().runs(reply.invocation_id()).await;
    assert_eq!(runs, [CLOCK_STEP, CLOCK_STEP, FETCH_STEP]);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs RESTATE_SERVER_BIN"]
async fn sleeps_durably_until_the_committed_round() {
    if launcher_or_skip(ReusePolicy::Never).is_none() {
        return;
    }
    let restate = launch(mock(Duration::from_secs(6))).await;
    let started = Instant::now();
    let reply = restate
        .invoke(&Call::service("Lottery", "draw"), None, None)
        .await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    // The round was 3-6 s in the future; the test transport serves it only once due.
    assert!(
        started.elapsed() >= Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    let journal = restate.admin().journal(reply.invocation_id()).await;
    assert!(
        journal.iter().any(|e| e.entry_type.contains("Sleep")),
        "{:#?}",
        journal.iter().map(|e| &e.entry_type).collect::<Vec<_>>()
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs RESTATE_SERVER_BIN"]
async fn unavailable_after_the_fetch_budget_then_retryable_on_the_same_round() {
    if launcher_or_skip(ReusePolicy::Never).is_none() {
        return;
    }
    // quicknet behind an unreachable relay.
    let client = Client::builder(chains::quicknet())
        .relays(vec![Relay::v1("http://127.0.0.1:9").unwrap()])
        .http_client(reqwest_client())
        .build()
        .unwrap();
    let drand = Drand::new(client)
        .with_lead(Duration::from_secs(3))
        .with_fetch_budget(Duration::from_secs(1));
    let restate = launch(drand).await;

    let reply = restate
        .invoke(&Call::service("Lottery", "draw"), None, None)
        .await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    let round = reply.body["unavailable"].as_u64().expect("unavailable");
    // Awaiting the same round again fetches right away (it is due).
    let started = Instant::now();
    let reply = restate
        .invoke(
            &Call::service("Lottery", "awaitRound"),
            Some(&json!(round)),
            None,
        )
        .await;
    assert_eq!(
        reply.body["unavailable"].as_u64(),
        Some(round),
        "{}",
        reply.body
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs RESTATE_SERVER_BIN"]
async fn cancellation_propagates_instead_of_reporting_unavailable() {
    if launcher_or_skip(ReusePolicy::Never).is_none() {
        return;
    }
    let restate = launch(mock(Duration::from_mins(2))).await;
    let sent = restate
        .invoke(&Call::service("Lottery", "draw").send(), None, None)
        .await;
    assert!(sent.status < 300, "{}", sent.body);
    let id = sent.invocation_id().to_owned();
    // Wait until it sleeps towards the committed round, then cancel.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let journal = restate.admin().journal(&id).await;
        if journal.iter().any(|e| e.entry_type.contains("Sleep")) {
            break;
        }
        assert!(Instant::now() < deadline, "no sleep in the journal");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    restate.admin().cancel(&id).await;
    restate.admin().await_status(&id, &["completed"]).await;
    let invocation = restate.admin().invocation(&id).await;
    let failure = invocation.completion_failure.unwrap_or_default();
    assert!(failure.contains("cancelled"), "{failure:?}");
}

fn reqwest_client() -> drand::client::reqwest::Client {
    drand::client::reqwest::Client::new()
}
