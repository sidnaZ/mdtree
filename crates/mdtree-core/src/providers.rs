//! Injectable providers for values that would otherwise make tests nondeterministic.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use ulid::Ulid;

/// Supplies the current time to domain and application services.
pub trait Clock: Send + Sync {
    /// Returns the current time as milliseconds since the Unix epoch.
    fn now_millis(&self) -> u64;
}

/// Clock backed by the operating system.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_millis(&self) -> u64 {
        let elapsed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock must not be before the Unix epoch");

        u64::try_from(elapsed.as_millis()).expect("timestamp must fit in u64 milliseconds")
    }
}

/// Clock that always returns one value, intended for deterministic tests.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FixedClock {
    now_millis: u64,
}

impl FixedClock {
    /// Creates a clock fixed at `now_millis` milliseconds since the Unix epoch.
    #[must_use]
    pub const fn new(now_millis: u64) -> Self {
        Self { now_millis }
    }
}

impl Clock for FixedClock {
    fn now_millis(&self) -> u64 {
        self.now_millis
    }
}

/// Supplies new ULIDs to domain and application services.
pub trait UlidGenerator: Send + Sync {
    /// Generates the next ULID.
    fn generate(&self) -> Ulid;
}

/// ULID generator backed by the crate's random production generator.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemUlidGenerator;

impl UlidGenerator for SystemUlidGenerator {
    fn generate(&self) -> Ulid {
        Ulid::gen()
    }
}

/// Repeatable ULID sequence intended for tests.
///
/// Values start at the supplied raw ULID value and increment atomically, so a
/// shared provider remains safe to inject into concurrent services.
#[derive(Debug)]
pub struct SequentialUlidGenerator {
    low: AtomicU64,
}

impl SequentialUlidGenerator {
    /// Creates a deterministic generator starting at the supplied raw value.
    #[must_use]
    pub const fn new(start: u64) -> Self {
        Self {
            low: AtomicU64::new(start),
        }
    }
}

impl UlidGenerator for SequentialUlidGenerator {
    fn generate(&self) -> Ulid {
        let low = self.low.fetch_add(1, Ordering::Relaxed);
        Ulid::from(u128::from(low))
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::{
        Clock, FixedClock, SequentialUlidGenerator, SystemClock, SystemUlidGenerator, UlidGenerator,
    };
    use ulid::Ulid;

    #[test]
    fn fixed_clock_returns_repeatable_timestamps() {
        let clock = FixedClock::new(1_725_000_123_456);

        assert_eq!(clock.now_millis(), 1_725_000_123_456);
        assert_eq!(clock.now_millis(), 1_725_000_123_456);
    }

    #[test]
    fn sequential_generator_returns_repeatable_ids() {
        let generator = SequentialUlidGenerator::new(42);

        assert_eq!(generator.generate(), Ulid::from(42_u128));
        assert_eq!(generator.generate(), Ulid::from(43_u128));
    }

    #[test]
    fn production_clock_returns_a_plausible_timestamp() {
        assert!(SystemClock.now_millis() > 1_700_000_000_000);
    }

    #[test]
    fn production_generator_returns_valid_ulids() {
        let id = SystemUlidGenerator.generate();

        assert_eq!(Ulid::from_str(&id.to_string()), Ok(id));
    }
}
