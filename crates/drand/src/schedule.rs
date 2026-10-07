use core::{num::NonZeroU32, time::Duration};

use crate::UnixTime;

/// Round timing of a chain. Pure: callers pass the current time.
///
/// Round 1 is published at `genesis_time`, round `r` at
/// `genesis_time + (r - 1) * period`. [`Schedule::round_at`] and
/// [`Schedule::round_time`] are drand-client's `roundAt` and `roundTime`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Schedule {
    genesis_time: i64,
    period: NonZeroU32,
}

impl Schedule {
    /// A schedule from genesis time and period, in seconds as drand publishes
    /// them.
    #[must_use]
    pub const fn new(genesis_time: i64, period: NonZeroU32) -> Self {
        Self {
            genesis_time,
            period,
        }
    }

    /// When round 1 is due.
    #[must_use]
    pub const fn genesis_time(&self) -> UnixTime {
        UnixTime::from_secs(self.genesis_time)
    }

    /// Time between rounds.
    #[must_use]
    pub const fn period(&self) -> Duration {
        Duration::from_secs(self.period.get() as u64)
    }

    /// The latest round due at or before `time`; `None` before genesis.
    #[must_use]
    pub fn round_at(&self, time: UnixTime) -> Option<u64> {
        let elapsed = i128::from(time.as_millis()) - i128::from(self.genesis_time) * 1000;
        if elapsed < 0 {
            return None;
        }
        let round = elapsed / (i128::from(self.period.get()) * 1000) + 1;
        u64::try_from(round).ok()
    }

    /// When `round` is due; `None` for round 0 or on overflow.
    #[must_use]
    pub fn round_time(&self, round: u64) -> Option<UnixTime> {
        let offset = round
            .checked_sub(1)?
            .checked_mul(u64::from(self.period.get()))?;
        let secs = self.genesis_time.checked_add(i64::try_from(offset).ok()?)?;
        secs.checked_mul(1000).map(UnixTime::from_millis)
    }

    /// The first round due strictly after `time` (`round_at(time) + 1`, or 1
    /// before genesis).
    ///
    /// Its randomness is unknown at `time`, assuming fewer than a threshold
    /// of drand nodes collude and the local clock is right. Leave at least one
    /// period of margin when committing to a round.
    #[must_use]
    pub fn round_after(&self, time: UnixTime) -> u64 {
        self.round_at(time).map_or(1, |r| r.saturating_add(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quicknet() -> Schedule {
        Schedule::new(1_692_803_367, NonZeroU32::new(3).unwrap())
    }

    #[test]
    fn boundaries() {
        let s = quicknet();
        let g = s.genesis_time();
        let at = |secs: i64| UnixTime::from_millis(g.as_millis() + secs * 1000);
        assert_eq!(s.period(), Duration::from_secs(3));
        assert_eq!(s.round_at(at(-1)), None);
        assert_eq!(s.round_at(g), Some(1));
        assert_eq!(s.round_at(at(2)), Some(1));
        assert_eq!(s.round_at(at(3)), Some(2));
        assert_eq!(s.round_time(0), None);
        assert_eq!(s.round_time(1), Some(g));
        assert_eq!(s.round_time(2), Some(at(3)));
        assert_eq!(s.round_after(at(-100)), 1);
        assert_eq!(s.round_after(g), 2);
        assert_eq!(s.round_at(UnixTime::from_millis(g.as_millis() - 1)), None);
        assert_eq!(
            s.round_at(UnixTime::from_millis(g.as_millis() + 2999)),
            Some(1)
        );
        assert_eq!(s.round_time(u64::MAX), None);
    }
}
