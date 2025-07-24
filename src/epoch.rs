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

#[derive(Clone, Copy, PartialEq, Eq)]
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

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Color {
    C0 = 0,
    C1 = 1,
}

impl From<usize> for Color {
    fn from(value: usize) -> Self {
        if value > 0 { Self::C0 } else { Self::C1 }
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
}
