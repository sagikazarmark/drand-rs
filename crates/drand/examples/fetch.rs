//! Fetches and verifies the latest quicknet beacon, then waits for the next one.
//!
//! ```sh
//! cargo run --example fetch --features client,rustls
//! ```

use std::time::Duration;

use drand::{chains, client::Client};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::builder(chains::quicknet()).build()?;

    let latest = client.latest().await?;
    println!(
        "latest: round {} randomness {}",
        latest.round(),
        hex::encode(latest.randomness())
    );

    let next = client
        .wait_for(latest.round() + 1, Duration::from_secs(30))
        .await?;
    println!(
        "next:   round {} randomness {}",
        next.round(),
        hex::encode(next.randomness())
    );
    Ok(())
}
