//! The global epoch

use std::sync::atomic::{AtomicUsize, Ordering};

/// An epoch that can be marked as pinned or unpinned.
///
/// Internally, the epoch is represented as an integer that wraps around at some unspecified point
/// and a flag that represents whether it is pinned or unpinned.
#[derive(Copy, Clone, Default, Debug, Eq, PartialEq)]
pub(crate) struct Epoch {
    /// The bits hold the following (from least to most significant):
    /// 1. pinned (1 bit): set if pinned.
    /// 2. phase number (2 bits): refers to the current phase (e.g., normal, tracing).
    /// 3. white color (1 bit): flips after a successful completion of a collection cycle.
    /// 4. timestamp (rest): increases on every phase transition.
    data: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    /// Normal phase.
    N = 0b00,
    /// Root tracing phase.
    RT = 0b01,
    /// Completion tracing phase.
    CT = 0b10,
}

impl From<usize> for Phase {
    fn from(value: usize) -> Self {
        match value {
            0b00 => Self::N,
            0b01 => Self::RT,
            0b10 => Self::CT,
            _ => panic!("Invalid phase number"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Color {
    C0 = 0,
    C1 = 1,
}

impl From<usize> for Color {
    fn from(value: usize) -> Self {
        if value == 0 { Self::C0 } else { Self::C1 }
    }
}

impl Color {
    pub fn flip(self) -> Self {
        match self {
            Color::C0 => Color::C1,
            Color::C1 => Color::C0,
        }
    }
}

impl Epoch {
    const PINNED_POS: usize = 0;
    const PHASE_NUMBER_POS: usize = Self::PINNED_POS + 1;
    const COLOR_POS: usize = Self::PHASE_NUMBER_POS + 2;
    const TIMESTAMP_POS: usize = Self::COLOR_POS + 1;

    const PINNED_BIT: usize = 0b1 << Self::PINNED_POS;
    const PHASE_NUMBER_BITS: usize = 0b11 << Self::PHASE_NUMBER_POS;
    const COLOR_BIT: usize = 0b1 << Self::COLOR_POS;
    const TIMESTAMP_BITS: usize = usize::MAX << Self::TIMESTAMP_POS;

    /// Returns the starting epoch in unpinned state.
    #[inline]
    pub(crate) fn starting() -> Self {
        Self::default()
    }

    /// Returns `true` if the epoch is marked as pinned.
    #[inline]
    pub(crate) fn is_pinned(self) -> bool {
        (self.data & Self::PINNED_BIT) > 0
    }

    /// Returns the same epoch, but marked as pinned.
    #[inline]
    pub(crate) fn pinned(self) -> Self {
        Self {
            data: self.data | Self::PINNED_BIT,
        }
    }

    /// Returns the same epoch, but marked as unpinned.
    #[inline]
    #[allow(unused)]
    pub(crate) fn unpinned(self) -> Self {
        Self {
            data: self.data & !Self::PINNED_BIT,
        }
    }

    #[inline]
    pub(crate) fn phase(self) -> Phase {
        ((self.data & Self::PHASE_NUMBER_BITS) >> Self::PHASE_NUMBER_POS).into()
    }

    #[inline]
    pub(crate) fn with_phase(self, phase: Phase) -> Self {
        Self {
            data: (self.data & !Self::PHASE_NUMBER_BITS)
                | ((phase as usize) << Self::PHASE_NUMBER_POS),
        }
    }

    #[inline]
    pub(crate) fn color(self) -> Color {
        ((self.data & Self::COLOR_BIT) >> Self::COLOR_POS).into()
    }

    #[inline]
    pub(crate) fn with_color(self, color: Color) -> Self {
        Self {
            data: (self.data & !Self::COLOR_BIT) | ((color as usize) << Self::COLOR_POS),
        }
    }

    #[inline]
    pub(crate) fn timestamp(self) -> usize {
        (self.data & Self::TIMESTAMP_BITS) >> Self::TIMESTAMP_POS
    }

    #[inline]
    pub(crate) fn with_timestamp(self, value: usize) -> Self {
        Self {
            data: (self.data & !Self::TIMESTAMP_BITS) | (value << Self::TIMESTAMP_POS),
        }
    }
}

/// An atomic value that holds an `Epoch`.
#[derive(Default, Debug)]
pub(crate) struct AtomicEpoch {
    /// Since `Epoch` is just a wrapper around `usize`, an `AtomicEpoch` is similarly represented
    /// using an `AtomicUsize`.
    data: AtomicUsize,
}

impl AtomicEpoch {
    /// Creates a new atomic epoch.
    #[inline]
    pub(crate) fn new(epoch: Epoch) -> Self {
        let data = AtomicUsize::new(epoch.data);
        Self { data }
    }

    /// Loads a value from the atomic epoch.
    #[inline]
    pub(crate) fn load(&self, ord: Ordering) -> Epoch {
        Epoch {
            data: self.data.load(ord),
        }
    }

    /// Stores a value into the atomic epoch.
    #[inline]
    pub(crate) fn store(&self, epoch: Epoch, ord: Ordering) {
        self.data.store(epoch.data, ord);
    }

    /// Stores a value into the atomic epoch if the current value is the same as `current`.
    ///
    /// The return value is a result indicating whether the new value was written and containing
    /// the previous value. On success this value is guaranteed to be equal to `current`.
    ///
    /// This method takes two `Ordering` arguments to describe the memory
    /// ordering of this operation. `success` describes the required ordering for the
    /// read-modify-write operation that takes place if the comparison with `current` succeeds.
    /// `failure` describes the required ordering for the load operation that takes place when
    /// the comparison fails. Using `Acquire` as success ordering makes the store part
    /// of this operation `Relaxed`, and using `Release` makes the successful load
    /// `Relaxed`. The failure ordering can only be `SeqCst`, `Acquire` or `Relaxed`
    /// and must be equivalent to or weaker than the success ordering.
    #[inline]
    #[allow(unused)]
    pub(crate) fn compare_exchange(
        &self,
        current: Epoch,
        new: Epoch,
        success: Ordering,
        failure: Ordering,
    ) -> Result<Epoch, Epoch> {
        match self
            .data
            .compare_exchange(current.data, new.data, success, failure)
        {
            Ok(data) => Ok(Epoch { data }),
            Err(data) => Err(Epoch { data }),
        }
    }

    /// Loads a value "non-atomically" from the atomic epoch.
    ///
    /// # Safety
    ///
    /// There must be no interleaving writes on this variable.
    ///
    /// Note that read-read races, where one access is atomic and one is not, are not UB.
    /// * https://doc.rust-lang.org/nightly/std/sync/atomic/index.html#memory-model-for-atomic-accesses
    #[inline]
    pub(crate) unsafe fn load_non_atomic(&self) -> Epoch {
        Epoch {
            data: unsafe { *self.data.as_ptr() },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Color tests ----

    #[test]
    fn color_flip() {
        assert_eq!(Color::C0.flip(), Color::C1);
        assert_eq!(Color::C1.flip(), Color::C0);
    }

    #[test]
    fn color_flip_roundtrip() {
        assert_eq!(Color::C0.flip().flip(), Color::C0);
        assert_eq!(Color::C1.flip().flip(), Color::C1);
    }

    #[test]
    fn color_from_usize() {
        assert_eq!(Color::from(0), Color::C0);
        assert_eq!(Color::from(1), Color::C1);
        // Non-zero values map to C1.
        assert_eq!(Color::from(42), Color::C1);
    }

    #[test]
    fn color_equality() {
        assert_eq!(Color::C0, Color::C0);
        assert_eq!(Color::C1, Color::C1);
        assert_ne!(Color::C0, Color::C1);
    }

    // ---- Phase tests ----

    #[test]
    fn phase_from_usize() {
        assert_eq!(Phase::from(0b00), Phase::N);
        assert_eq!(Phase::from(0b01), Phase::RT);
        assert_eq!(Phase::from(0b10), Phase::CT);
    }

    #[test]
    #[should_panic(expected = "Invalid phase number")]
    fn phase_from_invalid() {
        let _ = Phase::from(0b11);
    }

    #[test]
    fn phase_values() {
        assert_eq!(Phase::N as usize, 0b00);
        assert_eq!(Phase::RT as usize, 0b01);
        assert_eq!(Phase::CT as usize, 0b10);
    }

    // ---- Epoch tests ----

    #[test]
    fn epoch_starting_is_unpinned() {
        let e = Epoch::starting();
        assert!(!e.is_pinned());
        assert_eq!(e.phase(), Phase::N);
        assert_eq!(e.color(), Color::C0);
        assert_eq!(e.timestamp(), 0);
    }

    #[test]
    fn epoch_pinned_unpinned() {
        let e = Epoch::starting();
        assert!(!e.is_pinned());

        let pinned = e.pinned();
        assert!(pinned.is_pinned());

        let unpinned = pinned.unpinned();
        assert!(!unpinned.is_pinned());
    }

    #[test]
    fn epoch_pinned_preserves_fields() {
        let e = Epoch::starting()
            .with_phase(Phase::RT)
            .with_color(Color::C1)
            .with_timestamp(42);

        let pinned = e.pinned();
        assert!(pinned.is_pinned());
        assert_eq!(pinned.phase(), Phase::RT);
        assert_eq!(pinned.color(), Color::C1);
        assert_eq!(pinned.timestamp(), 42);
    }

    #[test]
    fn epoch_phase_transitions() {
        let e = Epoch::starting();
        assert_eq!(e.phase(), Phase::N);

        let rt = e.with_phase(Phase::RT);
        assert_eq!(rt.phase(), Phase::RT);

        let ct = rt.with_phase(Phase::CT);
        assert_eq!(ct.phase(), Phase::CT);

        let n = ct.with_phase(Phase::N);
        assert_eq!(n.phase(), Phase::N);
    }

    #[test]
    fn epoch_phase_preserves_other_fields() {
        let e = Epoch::starting()
            .pinned()
            .with_color(Color::C1)
            .with_timestamp(7);

        let rt = e.with_phase(Phase::RT);
        assert!(rt.is_pinned());
        assert_eq!(rt.color(), Color::C1);
        assert_eq!(rt.timestamp(), 7);
    }

    #[test]
    fn epoch_color() {
        let e = Epoch::starting();
        assert_eq!(e.color(), Color::C0);

        let c1 = e.with_color(Color::C1);
        assert_eq!(c1.color(), Color::C1);

        let c0 = c1.with_color(Color::C0);
        assert_eq!(c0.color(), Color::C0);
    }

    #[test]
    fn epoch_color_preserves_other_fields() {
        let e = Epoch::starting()
            .pinned()
            .with_phase(Phase::CT)
            .with_timestamp(99);

        let flipped = e.with_color(Color::C1);
        assert!(flipped.is_pinned());
        assert_eq!(flipped.phase(), Phase::CT);
        assert_eq!(flipped.timestamp(), 99);
    }

    #[test]
    fn epoch_timestamp() {
        let e = Epoch::starting();
        assert_eq!(e.timestamp(), 0);

        let t1 = e.with_timestamp(1);
        assert_eq!(t1.timestamp(), 1);

        let t100 = e.with_timestamp(100);
        assert_eq!(t100.timestamp(), 100);
    }

    #[test]
    fn epoch_timestamp_preserves_other_fields() {
        let e = Epoch::starting()
            .pinned()
            .with_phase(Phase::RT)
            .with_color(Color::C1);

        let ts = e.with_timestamp(55);
        assert!(ts.is_pinned());
        assert_eq!(ts.phase(), Phase::RT);
        assert_eq!(ts.color(), Color::C1);
    }

    #[test]
    fn epoch_all_fields_combined() {
        let e = Epoch::starting()
            .with_phase(Phase::CT)
            .with_color(Color::C1)
            .with_timestamp(12345)
            .pinned();

        assert!(e.is_pinned());
        assert_eq!(e.phase(), Phase::CT);
        assert_eq!(e.color(), Color::C1);
        assert_eq!(e.timestamp(), 12345);

        // Change each field independently and verify.
        let e2 = e.unpinned();
        assert!(!e2.is_pinned());
        assert_eq!(e2.phase(), Phase::CT);
        assert_eq!(e2.color(), Color::C1);
        assert_eq!(e2.timestamp(), 12345);
    }

    #[test]
    fn epoch_equality() {
        let a = Epoch::starting().with_phase(Phase::RT).with_timestamp(5);
        let b = Epoch::starting().with_phase(Phase::RT).with_timestamp(5);
        assert_eq!(a, b);

        let c = a.with_timestamp(6);
        assert_ne!(a, c);
    }

    // ---- AtomicEpoch tests ----

    #[test]
    fn atomic_epoch_load_store() {
        let ae = AtomicEpoch::default();
        let e = ae.load(Ordering::SeqCst);
        assert_eq!(e, Epoch::starting());

        let new = Epoch::starting()
            .with_phase(Phase::RT)
            .with_color(Color::C1)
            .with_timestamp(3)
            .pinned();
        ae.store(new, Ordering::SeqCst);

        let loaded = ae.load(Ordering::SeqCst);
        assert_eq!(loaded, new);
    }

    #[test]
    fn atomic_epoch_compare_exchange_success() {
        let initial = Epoch::starting().with_timestamp(1);
        let ae = AtomicEpoch::new(initial);

        let new = initial.with_phase(Phase::RT);
        let result = ae.compare_exchange(initial, new, Ordering::SeqCst, Ordering::SeqCst);
        assert!(result.is_ok());
        assert_eq!(ae.load(Ordering::SeqCst), new);
    }

    #[test]
    fn atomic_epoch_compare_exchange_failure() {
        let initial = Epoch::starting().with_timestamp(1);
        let ae = AtomicEpoch::new(initial);

        // Change the value first.
        let updated = initial.with_phase(Phase::CT);
        ae.store(updated, Ordering::SeqCst);

        // CAS with stale value should fail.
        let new = initial.with_phase(Phase::RT);
        let result = ae.compare_exchange(initial, new, Ordering::SeqCst, Ordering::SeqCst);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), updated);
    }

    #[test]
    fn atomic_epoch_load_non_atomic() {
        let e = Epoch::starting().with_phase(Phase::CT).with_timestamp(77);
        let ae = AtomicEpoch::new(e);

        // Safety: no concurrent writes in this single-threaded test.
        let loaded = unsafe { ae.load_non_atomic() };
        assert_eq!(loaded, e);
    }
}
