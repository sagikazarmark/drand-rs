# restate-drand

[![crates.io](https://img.shields.io/crates/v/restate-drand?style=flat-square)](https://crates.io/crates/restate-drand)
[![docs.rs](https://img.shields.io/docsrs/restate-drand?style=flat-square)](https://docs.rs/restate-drand)

**[drand](https://drand.love) randomness in [Restate](https://restate.dev) handlers: commit to a future
round, wait durably, fetch and verify the beacon.**

Keep a `Drand` in your service and pass the handler's context (the example also uses the `hex` crate):

```rust
use restate_drand::{AwaitError, Drand};
use restate_sdk::prelude::*;

struct Lottery {
    drand: Drand, // Drand::new(drand::client::Client::builder(chains::quicknet()).build()?)
}

#[restate_sdk::service]
impl Lottery {
    #[handler]
    async fn draw(&self, ctx: Context<'_>) -> HandlerResult<String> {
        let commitment = self.drand.commit_round(&ctx).await?;
        // … publish the commitment before the round exists …
        match self.drand.await_beacon(&ctx, commitment.round).await {
            Ok(beacon) => Ok(hex::encode(beacon.randomness())),
            // Keep the commitment and await the same round later; never draw again on a new one.
            Err(e @ AwaitError::Unavailable { .. }) => Ok(format!("try again later: {e}")),
            Err(e) => Err(e.into_handler_error()),
        }
    }
}
```

- **`commit_round(&ctx)`** reads the clock as a journaled step and commits to the latest round due at
  `now + lead` (default: two periods). A replay commits to the same round. The `Commitment` is serializable,
  so it can be published as is.
- **`await_beacon(&ctx, round)`** sleeps durably until the round is due, then fetches it in a journaled step,
  retried for the fetch budget (default one minute, set with `with_fetch_budget`). It verifies the journaled beacon against the chain with
  [`drand`](https://crates.io/crates/drand).
- **Errors.**
  - `AwaitError::Unavailable` means no valid beacon arrived within the budget. The round exists, or will: keep
    the commitment and call `await_beacon` again for the same round. Redrawing on a new round would let a relay
    that dislikes a published round force a redraw.
  - `AwaitError::Terminal` is round 0, a cancellation or a failed step: propagate it with `into_handler_error()`.
    Don't use `?`: the SDK turns any `std::error::Error` into a *retryable* handler error.
- **Contexts.** It works with all five SDK contexts (`Context`, `ObjectContext`, `SharedObjectContext`,
  `WorkflowContext`, `SharedWorkflowContext`).
- **Journal steps.** They are named `drand: clock` and `drand: fetch` (`CLOCK_STEP`, `FETCH_STEP`). The names
  are part of the journal format: changing them would break the replay of invocations in flight.

## Features

TLS for the built-in drand client: `default-tls` (default, reqwest's default backend: rustls), `rustls` or
`native-tls`. To pick a backend yourself, set `default-features = false` and enable one, or pass your own
`reqwest::Client` with `.http_client(..)`.

The API exposes `drand` (re-exported), `restate-sdk` 0.12 and `restate-ext` 0.3 types, so a breaking upgrade of any
of them is a breaking release of this crate.

## WebAssembly (Cloudflare Workers)

It builds for `wasm32-unknown-unknown`, where the drand client runs on the JS host's `fetch` and timers (no
tokio, and the TLS features do nothing). `restate-sdk` needs `default-features = false` there, and its JWT
dependency needs getrandom's JS backend:

```toml
restate-sdk = { version = "0.12", default-features = false }
getrandom = { version = "0.2", features = ["js"] }
```

## Tests

End to end against a real `restate-server` (commit and await, the durable sleep, the fetch budget, cancellation):

```sh
RESTATE_SERVER_BIN=/path/to/restate-server cargo test -p restate-drand -- --ignored
```

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
