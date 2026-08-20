//! A clock that never reads the wall clock, and a seeded generator.

use core::cell::Cell;

use crate::traits::{Clock, Rng};

/// Default number of microseconds each read advances the clock by.
const DEFAULT_TICK: i64 = 1;

/// A logical clock: every read returns the previous value plus a fixed tick.
///
/// It satisfies the only property the engine needs from time — that it moves
/// forward — while keeping runs reproducible. A simulation can also drive it by
/// hand with [`LogicalClock::advance`] to test timing-sensitive behaviour
/// without sleeping.
#[derive(Debug)]
pub struct LogicalClock {
    now: Cell<i64>,
    tick: i64,
}

impl LogicalClock {
    /// A clock starting at zero and advancing one microsecond per read.
    pub fn new() -> Self {
        Self {
            now: Cell::new(0),
            tick: DEFAULT_TICK,
        }
    }

    /// A clock starting at `start` and advancing `tick` microseconds per read.
    pub fn with_tick(start: i64, tick: i64) -> Self {
        Self {
            now: Cell::new(start),
            tick,
        }
    }

    /// Move the clock forward by `micros` without reading it.
    pub fn advance(&self, micros: i64) {
        self.now.set(self.now.get().saturating_add(micros));
    }
}

impl Default for LogicalClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for LogicalClock {
    fn now_micros(&self) -> i64 {
        let now = self.now.get();
        self.now.set(now.saturating_add(self.tick));
        now
    }
}

/// A seeded xorshift64* generator.
///
/// Small and reproducible — the point is that a simulation replays exactly, not
/// that the stream is cryptographically strong. Do not use it for anything
/// security-sensitive.
#[derive(Debug, Clone)]
pub struct SeededRng {
    state: u64,
}

impl SeededRng {
    /// Create a generator from a seed. Seed 0 is remapped, since xorshift is
    /// stuck at zero.
    pub fn new(seed: u64) -> Self {
        Self {
            state: if seed == 0 {
                0x9E37_79B9_7F4A_7C15
            } else {
                seed
            },
        }
    }

    /// A float in `[0, 1)`.
    pub fn next_f32(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u32 << 24) as f32
    }
}

impl Rng for SeededRng {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    #[test]
    fn the_clock_only_moves_forward() {
        let clock = LogicalClock::new();
        let first = clock.now_micros();
        let second = clock.now_micros();
        assert!(second > first);
    }

    #[test]
    fn the_clock_is_reproducible() {
        let a = LogicalClock::new();
        let b = LogicalClock::new();
        assert_eq!(a.now_micros(), b.now_micros());
        assert_eq!(a.now_micros(), b.now_micros());
    }

    #[test]
    fn advancing_skips_time_without_a_read() {
        let clock = LogicalClock::with_tick(0, 1);
        clock.advance(1_000);
        assert_eq!(clock.now_micros(), 1_000);
    }

    #[test]
    fn the_same_seed_replays_the_same_stream() {
        let draw = |seed| {
            let mut rng = SeededRng::new(seed);
            (0..8).map(|_| rng.next_u64()).collect::<Vec<_>>()
        };
        assert_eq!(draw(42), draw(42));
        assert_ne!(draw(42), draw(43));
    }

    #[test]
    fn floats_stay_in_range() {
        let mut rng = SeededRng::new(7);
        for _ in 0..1_000 {
            let value = rng.next_f32();
            assert!((0.0..1.0).contains(&value), "out of range: {value}");
        }
    }
}
