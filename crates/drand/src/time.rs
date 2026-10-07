use core::time::Duration;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A point in time, in milliseconds since the Unix epoch (negative before it).
///
/// `no_std` stand-in for `std::time::SystemTime`; with the `std` feature it
/// converts from one. Serializes as an integer number of milliseconds, like
/// JavaScript's `Date.now()`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UnixTime(i64);

impl UnixTime {
    /// 1970-01-01T00:00:00Z.
    pub const UNIX_EPOCH: Self = Self(0);

    /// The time `secs` seconds after the epoch, saturating at the bounds.
    #[must_use]
    pub const fn from_secs(secs: i64) -> Self {
        Self(secs.saturating_mul(1000))
    }

    /// The time `millis` milliseconds after the epoch.
    #[must_use]
    pub const fn from_millis(millis: i64) -> Self {
        Self(millis)
    }

    /// Whole seconds since the epoch, rounded down.
    #[must_use]
    pub const fn as_secs(self) -> i64 {
        self.0.div_euclid(1000)
    }

    /// Milliseconds since the epoch.
    #[must_use]
    pub const fn as_millis(self) -> i64 {
        self.0
    }

    /// `self + duration` (to the millisecond), or `None` on overflow.
    #[must_use]
    pub fn checked_add(self, duration: Duration) -> Option<Self> {
        let millis = i64::try_from(duration.as_millis()).ok()?;
        self.0.checked_add(millis).map(Self)
    }

    /// `self + duration` (to the millisecond), saturating on overflow.
    #[must_use]
    pub fn saturating_add(self, duration: Duration) -> Self {
        self.checked_add(duration).unwrap_or(Self(i64::MAX))
    }

    /// How long after `earlier` this is; zero if `earlier` is later.
    #[must_use]
    pub fn saturating_duration_since(self, earlier: Self) -> Duration {
        u64::try_from(i128::from(self.0) - i128::from(earlier.0))
            .map_or(Duration::ZERO, Duration::from_millis)
    }
}

#[cfg(feature = "std")]
impl From<std::time::SystemTime> for UnixTime {
    /// Truncates to the millisecond, saturating at the bounds.
    fn from(t: std::time::SystemTime) -> Self {
        match t.duration_since(std::time::UNIX_EPOCH) {
            Ok(d) => Self(i64::try_from(d.as_millis()).unwrap_or(i64::MAX)),
            Err(e) => Self(i64::try_from(e.duration().as_millis()).map_or(i64::MIN, |ms| -ms)),
        }
    }
}

impl Serialize for UnixTime {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(s)
    }
}

impl<'de> Deserialize<'de> for UnixTime {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        i64::deserialize(d).map(Self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arithmetic() {
        let t = UnixTime::from_secs(10);
        assert_eq!(t.as_millis(), 10_000);
        assert_eq!(UnixTime::from_millis(-1).as_secs(), -1);
        assert_eq!(
            t.checked_add(Duration::from_millis(1500)),
            Some(UnixTime::from_millis(11_500))
        );
        assert_eq!(
            UnixTime::from_millis(i64::MAX).checked_add(Duration::from_millis(1)),
            None
        );
        assert_eq!(
            UnixTime::from_millis(i64::MAX).saturating_add(Duration::MAX),
            UnixTime::from_millis(i64::MAX)
        );
        assert_eq!(
            t.saturating_duration_since(UnixTime::UNIX_EPOCH),
            Duration::from_secs(10)
        );
        assert_eq!(
            UnixTime::UNIX_EPOCH.saturating_duration_since(t),
            Duration::ZERO
        );
        assert_eq!(UnixTime::from_secs(i64::MIN).as_millis(), i64::MIN);
    }

    #[test]
    fn serializes_as_millis() {
        let t = UnixTime::from_millis(1_692_803_367_000);
        let json = serde_json::to_string(&t).unwrap();
        assert_eq!(json, "1692803367000");
        assert_eq!(serde_json::from_str::<UnixTime>(&json).unwrap(), t);
    }
}
