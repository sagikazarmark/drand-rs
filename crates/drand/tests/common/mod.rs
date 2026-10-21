//! Shared by the client tests: a deterministic test chain and a settable clock.

#![allow(dead_code)]

use std::{
    num::NonZeroU32,
    sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    },
};

use drand::{Beacon, Scheme, TestChain, UnixTime, client::Clock, http::GRACE};

pub const GENESIS: i64 = 1_700_000_000;

pub fn test_chain() -> TestChain {
    TestChain::new(
        [3; 32],
        Scheme::BlsUnchainedG1Rfc9380,
        NonZeroU32::new(3).unwrap(),
        GENESIS,
        "mock",
    )
    .unwrap()
}

/// A settable clock.
#[derive(Clone, Default)]
pub struct FixedClock(Arc<AtomicI64>);

impl FixedClock {
    pub fn at(ms: i64) -> Self {
        Self(Arc::new(AtomicI64::new(ms)))
    }
    pub fn set(&self, ms: i64) {
        self.0.store(ms, Ordering::SeqCst);
    }
}

impl Clock for FixedClock {
    fn now(&self) -> UnixTime {
        UnixTime::from_millis(self.0.load(Ordering::SeqCst))
    }
}

pub fn grace_ms() -> i64 {
    i64::try_from(GRACE.as_millis()).unwrap()
}

pub fn due_ms(round: u64) -> i64 {
    test_chain()
        .chain()
        .schedule()
        .round_time(round)
        .unwrap()
        .as_millis()
}

pub fn signed(round: u64) -> Beacon {
    test_chain().sign(round)
}
