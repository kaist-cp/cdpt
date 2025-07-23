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

pub(crate) enum Phase {
    /// Normal phase.
    N = 0,
    /// Root tracing phase.
    RT = 1,
    /// Completion tracing phase.
    CT = 2,
}

#[derive(Clone, Copy)]
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
    /// Returns the starting epoch in unpinned state.
    #[inline]
    pub(crate) fn starting() -> Self {
        Self::default()
    }

    /// Returns `true` if the epoch is marked as pinned.
    #[inline]
    pub(crate) fn is_pinned(self) -> bool {
        (self.data & 1) == 1
    }

    /// Returns the same epoch, but marked as pinned.
    #[inline]
    pub(crate) fn pinned(self) -> Self {
        Self {
            data: self.data | 1,
        }
    }

    /// Returns the same epoch, but marked as unpinned.
    #[inline]
    pub(crate) fn unpinned(self) -> Self {
        Self {
            data: self.data & !1,
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
