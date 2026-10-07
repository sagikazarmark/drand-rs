//! [drand](https://drand.love) randomness in [Restate](https://restate.dev) handlers.
//!
//! Using public randomness fairly takes two steps, and both have to survive a
//! crash or a redeploy:
//!
//! 1. **Commit** to a round that is not published yet ([`Drand::commit_round`]):
//!    one journaled clock read, so a replay commits to the very same round.
//! 2. **Await** its beacon ([`Drand::await_beacon`]): a durable sleep until the
//!    round is due, a journaled fetch retried for [`Drand::fetch_budget`], and
//!    BLS verification of the journaled beacon (with [`drand`]).
//!
//! Keep a [`Drand`] in the service and pass the handler's context:
//!
//! ```no_run
//! use restate_drand::{AwaitError, Drand};
//! use restate_sdk::prelude::*;
//!
//! struct Lottery {
//!     drand: Drand,
//! }
//!
//! #[restate_sdk::service]
//! impl Lottery {
//!     #[handler]
//!     async fn draw(&self, ctx: Context<'_>) -> HandlerResult<String> {
//!         let commitment = self.drand.commit_round(&ctx).await?;
//!         // … publish the commitment before the round exists …
//!         match self.drand.await_beacon(&ctx, commitment.round).await {
//!             Ok(beacon) => Ok(hex::encode(beacon.randomness())),
//!             // Never draw again on a new round here: a relay that dislikes a
//!             // published round could otherwise force a redraw. Keep the
//!             // commitment and try the same round later.
//!             Err(e @ AwaitError::Unavailable { .. }) => Ok(format!("try again later: {e}")),
//!             Err(e) => Err(e.into_handler_error()),
//!         }
//!     }
//! }
//! ```
//!
//! # Journal
//!
//! Each call journals named steps ([`CLOCK_STEP`], [`FETCH_STEP`]), so they show
//! up in Restate's UI and a replay that meets different steps fails instead of
//! reading the wrong entry. The fetch journals the plain [`Beacon`] in the v1
//! JSON shape, and [`Drand::await_beacon`] verifies it again on every run.

#![cfg_attr(docsrs, feature(doc_cfg))]

use std::{future::Future, num::NonZeroU64, time::Duration};

pub use drand;
use drand::{
    Beacon, ChainInfo, UnixTime, VerifiedBeacon,
    client::{Client, Fetch},
};
use restate_ext::ContextClockExt;
use restate_sdk::prelude::*;

/// The name of the journaled clock reads. Part of the journal format: changing
/// it breaks the replay of invocations in flight.
pub const CLOCK_STEP: &str = "drand: clock";
/// The name of the journaled beacon fetch. Part of the journal format, like
/// [`CLOCK_STEP`].
pub const FETCH_STEP: &str = "drand: fetch";

/// Slack after a round's nominal time before the first fetch: relays publish a
/// little after it.
const SETTLE: Duration = Duration::from_millis(300);

/// A committed round.
///
/// Serializes as `{"round": …, "due_at": …}`, with `due_at` in unix
/// milliseconds (see [`UnixTime`]).
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Commitment {
    /// The round the caller committed to.
    pub round: u64,
    /// When the round is due.
    pub due_at: UnixTime,
}

/// Why [`Drand::await_beacon`] returned without a beacon.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum AwaitError {
    /// No valid beacon within [`Drand::fetch_budget`]. The round still exists
    /// (or will): keep the commitment and await the same round later.
    #[error("drand round {round} is unavailable: {reason}")]
    Unavailable {
        /// The awaited round.
        round: u64,
        /// The last failure.
        reason: String,
    },
    /// Round 0 was awaited, the invocation was cancelled, or a journaled step
    /// failed terminally.
    #[error("{} (code {})", .0.message(), .0.code())]
    Terminal(TerminalError),
}

impl AwaitError {
    /// Whether the invocation was cancelled.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Terminal(e) if is_cancellation(e))
    }

    /// The handler error to return: terminal in both cases. `Unavailable`
    /// becomes a 503 so the caller can retry the invocation.
    ///
    /// Use this rather than `?`: the SDK turns any `std::error::Error` into a
    /// *retryable* handler error, which would retry a cancelled invocation.
    #[must_use]
    pub fn into_handler_error(self) -> HandlerError {
        match self {
            e @ Self::Unavailable { .. } => TerminalError::new_with_code(503, e.to_string()).into(),
            Self::Terminal(e) => e.into(),
        }
    }
}

fn is_cancellation(e: &TerminalError) -> bool {
    e.code() == 409 && e.message() == "cancelled"
}

/// drand for one chain, kept in a Restate service.
#[derive(Clone, Debug)]
pub struct Drand {
    client: Client,
    lead: Duration,
    fetch_budget: Duration,
}

impl Drand {
    /// Wraps a [`drand`] client. Defaults: a lead of two periods (a full
    /// period of margin) and a one-minute fetch budget.
    #[must_use]
    pub fn new(client: Client) -> Self {
        let period = client.chain().period();
        Self {
            client,
            lead: 2 * period,
            fetch_budget: Duration::from_mins(1),
        }
    }

    /// How far ahead [`Drand::commit_round`] commits. With less than one
    /// period the committed round may already be public; zero commits to the
    /// latest published round (tests only).
    #[must_use]
    pub fn with_lead(mut self, lead: Duration) -> Self {
        self.lead = lead;
        self
    }

    /// How long [`Drand::await_beacon`] keeps retrying the fetch once the
    /// round is due (across attempts, not per request).
    #[must_use]
    pub fn with_fetch_budget(mut self, fetch_budget: Duration) -> Self {
        self.fetch_budget = fetch_budget;
        self
    }

    /// The underlying client.
    #[must_use]
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// The chain.
    #[must_use]
    pub fn chain(&self) -> &ChainInfo {
        self.client.chain()
    }

    /// The commit lead.
    #[must_use]
    pub fn lead(&self) -> Duration {
        self.lead
    }

    /// The fetch budget.
    #[must_use]
    pub fn fetch_budget(&self) -> Duration {
        self.fetch_budget
    }

    /// Commits to the latest round due at `now + lead`, reading the clock as a
    /// journaled step: a replay commits to the same round.
    ///
    /// # Errors
    ///
    /// The journaled clock read's error, e.g. when the invocation is cancelled.
    pub async fn commit_round<C: DrandContext>(
        &self,
        ctx: &C,
    ) -> Result<Commitment, TerminalError> {
        let now = UnixTime::from(ctx.now_named(CLOCK_STEP).await?);
        let schedule = self.chain().schedule();
        let round = schedule
            .round_at(now.saturating_add(self.lead))
            .unwrap_or(1);
        Ok(Commitment {
            round,
            due_at: schedule
                .round_time(round)
                .unwrap_or(UnixTime::from_millis(i64::MAX)),
        })
    }

    /// Sleeps durably until `round` is due, fetches it (journaled, retried for
    /// [`Drand::fetch_budget`]) and verifies it.
    ///
    /// Call it again for the same round after [`AwaitError::Unavailable`]: if
    /// the round is already due it fetches right away.
    ///
    /// # Errors
    ///
    /// [`AwaitError::Unavailable`] if no valid beacon arrived within
    /// [`Drand::fetch_budget`], and [`AwaitError::Terminal`] for round 0, a
    /// cancellation, or a failed journaled step.
    pub async fn await_beacon<C: DrandContext>(
        &self,
        ctx: &C,
        round: u64,
    ) -> Result<VerifiedBeacon, AwaitError> {
        let Some(round) = NonZeroU64::new(round) else {
            return Err(AwaitError::Terminal(TerminalError::new_with_code(
                400,
                "drand round 0 does not exist",
            )));
        };
        let now = UnixTime::from(
            ctx.now_named(CLOCK_STEP)
                .await
                .map_err(AwaitError::Terminal)?,
        );
        if let Some(due) = self.chain().schedule().round_time(round.get()) {
            let wait = due.saturating_duration_since(now);
            if !wait.is_zero() {
                ctx.drand_sleep(wait + SETTLE)
                    .await
                    .map_err(AwaitError::Terminal)?;
            }
        }
        let retry = RunRetryPolicy::new()
            .initial_delay(Duration::from_millis(250))
            .exponentiation_factor(2.0)
            .max_delay(Duration::from_secs(2))
            .max_duration(self.fetch_budget);
        match ctx.drand_fetch(self.client.clone(), round, retry).await {
            Ok(beacon) => self
                .client
                .verifier()
                .verify_round(round.get(), &beacon)
                .map_err(|e| AwaitError::Unavailable {
                    round: round.get(),
                    reason: format!("invalid beacon: {e}"),
                }),
            Err(e) if is_cancellation(&e) => Err(AwaitError::Terminal(e)),
            Err(e) => Err(AwaitError::Unavailable {
                round: round.get(),
                reason: e.message().to_owned(),
            }),
        }
    }
}

/// A Restate context [`Drand`] can work with: any of the SDK's five.
///
/// Sealed, with one impl per context (like `restate_ext`'s clock): the SDK does
/// not promise its `run` future is `Send`, and only a concrete context lets the
/// compiler see that it is.
pub trait DrandContext: sealed::Sealed + ContextClockExt + Sync {
    #[doc(hidden)]
    fn drand_sleep(
        &self,
        duration: Duration,
    ) -> impl Future<Output = Result<(), TerminalError>> + Send;

    #[doc(hidden)]
    fn drand_fetch(
        &self,
        client: Client,
        round: NonZeroU64,
        retry: RunRetryPolicy,
    ) -> impl Future<Output = Result<Beacon, TerminalError>> + Send;
}

mod sealed {
    pub trait Sealed {}
}

macro_rules! impl_drand_context {
    ($($context:ident),+ $(,)?) => {
        $(
            impl sealed::Sealed for $context<'_> {}

            impl DrandContext for $context<'_> {
                fn drand_sleep(
                    &self,
                    duration: Duration,
                ) -> impl Future<Output = Result<(), TerminalError>> + Send {
                    self.sleep(duration)
                }

                fn drand_fetch(
                    &self,
                    client: Client,
                    round: NonZeroU64,
                    retry: RunRetryPolicy,
                ) -> impl Future<Output = Result<Beacon, TerminalError>> + Send {
                    let step = self
                        .run(move || async move { fetch_step(&client, round).await })
                        .name(FETCH_STEP)
                        .retry_policy(retry);
                    async move { step.await.map(|Json(beacon)| beacon) }
                }
            }
        )+
    };
}

impl_drand_context!(
    Context,
    ObjectContext,
    SharedObjectContext,
    WorkflowContext,
    SharedWorkflowContext,
);

/// One fetch attempt: "not yet" and relay failures are retryable (a due round
/// always comes into existence eventually); anything else is terminal.
async fn fetch_step(client: &Client, round: NonZeroU64) -> HandlerResult<Json<Beacon>> {
    match client.fetch(round.get()).await {
        Ok(Fetch::Ready(beacon)) => Ok(Json(beacon.into_beacon())),
        Ok(_) => Err(anyhow::anyhow!("drand round {round} is not published yet").into()),
        Err(e) if e.is_retryable() => Err(anyhow::Error::new(e).into()),
        Err(e) => Err(TerminalError::new(e.to_string()).into()),
    }
}
