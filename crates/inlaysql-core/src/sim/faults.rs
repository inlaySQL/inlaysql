//! Deterministic schedules of [`Fault`]s to inject at sync time.
//!
//! A schedule is the harness's idea of "the universe deciding when to fail".
//! It can be an explicit script (useful for a hand-crafted adversarial test) or
//! a seeded random stream (useful for sweeping thousands of scenarios in CI).
//! Either way the sequence is a pure function of its inputs, so a run replays
//! byte-for-byte from a seed.

use alloc::collections::VecDeque;

use super::disk::Fault;
use crate::mem::SeededRng;
use crate::traits::Rng;

/// A stream of [`Fault`]s, one consumed per sync.
///
/// Values are drawn lazily because the number of syncs a workload will issue is
/// not known up front; only the seed (and the generation rule) is fixed, which
/// is enough to make the whole stream deterministic.
#[derive(Debug, Clone)]
pub struct FaultSchedule {
    /// An explicit script, consumed in order; when it is empty the schedule
    /// yields [`Fault::None`] forever (or draws from `random` if present).
    script: VecDeque<Fault>,
    /// Seeded random source, used when `script` is exhausted.
    random: Option<SeededRng>,
    /// Chances, in parts per thousand, of each fault when drawing randomly.
    crash_chance: u32,
    torn_chance: u32,
    reorder_chance: u32,
    /// How many faults have been yielded.
    drawn: u64,
}

impl FaultSchedule {
    /// A schedule that yields exactly `faults` in order, then [`Fault::None`].
    pub fn script(faults: &[Fault]) -> Self {
        Self {
            script: faults.iter().copied().collect(),
            random: None,
            crash_chance: 0,
            torn_chance: 0,
            reorder_chance: 0,
            drawn: 0,
        }
    }

    /// A schedule that draws random faults from `seed` using the default
    /// chances (1% crash, 1% torn write, 1% reordered sync per sync).
    pub fn random(seed: u64) -> Self {
        Self::random_with(seed, 10, 10, 10)
    }

    /// A schedule that draws random faults from `seed` with explicit chances,
    /// each in parts per thousand, summed across the three fault kinds.
    pub fn random_with(seed: u64, crash: u32, torn: u32, reorder: u32) -> Self {
        Self {
            script: VecDeque::new(),
            random: Some(SeededRng::new(seed)),
            crash_chance: crash,
            torn_chance: torn,
            reorder_chance: reorder,
            drawn: 0,
        }
    }

    /// The next fault to apply. Deterministic given the schedule's inputs.
    pub fn next_fault(&mut self) -> Fault {
        self.drawn += 1;
        if let Some(fault) = self.script.pop_front() {
            return fault;
        }
        let Some(rng) = &mut self.random else {
            return Fault::None;
        };

        let roll = (rng.next_f32() * 1000.0) as u32;
        let crash_end = self.crash_chance;
        let torn_end = crash_end + self.torn_chance;
        let reorder_end = torn_end + self.reorder_chance;

        if roll < crash_end {
            Fault::Crash
        } else if roll < torn_end {
            // A torn write loses between one byte and a whole block; bound it
            // by something a page write could plausibly exceed.
            let prefix = 1 + (rng.next_u64() as usize) % 4096;
            Fault::TornWrite { prefix }
        } else if roll < reorder_end {
            let syncs_ago = (rng.next_u64() as usize) % 4;
            Fault::ReorderedSync { syncs_ago }
        } else {
            Fault::None
        }
    }

    /// How many faults have been yielded so far.
    pub fn drawn(&self) -> u64 {
        self.drawn
    }
}

impl Default for FaultSchedule {
    fn default() -> Self {
        Self::script(&[])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    #[test]
    fn a_script_replays_in_order_then_turns_inert() {
        let mut schedule = FaultSchedule::script(&[Fault::Crash, Fault::TornWrite { prefix: 2 }]);
        assert_eq!(schedule.next_fault(), Fault::Crash);
        assert_eq!(schedule.next_fault(), Fault::TornWrite { prefix: 2 });
        assert_eq!(schedule.next_fault(), Fault::None);
        assert_eq!(schedule.next_fault(), Fault::None);
    }

    #[test]
    fn the_same_seed_replays_the_same_stream() {
        let draw = |seed| {
            let mut schedule = FaultSchedule::random(seed);
            (0..64).map(|_| schedule.next_fault()).collect::<Vec<_>>()
        };
        assert_eq!(draw(9), draw(9));
        assert_ne!(draw(9), draw(10));
    }

    #[test]
    fn a_seed_that_always_crashes_never_tears() {
        let mut schedule = FaultSchedule::random_with(1, 1000, 0, 0);
        for _ in 0..32 {
            assert_eq!(schedule.next_fault(), Fault::Crash);
        }
    }

    #[test]
    fn drawn_counts_faults_not_syncs() {
        let mut schedule = FaultSchedule::script(&[Fault::None]);
        assert_eq!(schedule.drawn(), 0);
        schedule.next_fault();
        assert_eq!(schedule.drawn(), 1);
    }
}
