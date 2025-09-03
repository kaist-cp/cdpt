use core::marker::PhantomData;
use core::mem::{self, MaybeUninit};
use core::ptr;
use std::mem::forget;

use crate::Guard;

/// Number of words a piece of `Data` can hold.
const DATA_WORDS: usize = 1;

/// Some space to keep a `FnOnce()` object on the stack.
type Data = [usize; DATA_WORDS];

/// A `FnOnce()` that is stored inline if small, or otherwise boxed on the heap.
///
/// This is a handy way of keeping an unsized `FnOnce()` within a sized structure.
pub(crate) struct Task {
    call: unsafe fn(*mut u8, &Guard),
    data: MaybeUninit<Data>,
    _marker: PhantomData<*mut ()>, // !Send + !Sync
}

unsafe impl Send for Task {}

impl Task {
    /// Constructs a new `Task` from a `FnOnce()`.
    pub(crate) fn new<F: FnOnce(&Guard)>(f: F) -> Self {
        let size = mem::size_of::<F>();
        let align = mem::align_of::<F>();

        assert!(
            size <= mem::size_of::<Data>() && align <= mem::align_of::<Data>(),
            "Increase `DATA_WORDS`"
        );
        unsafe {
            let mut data = MaybeUninit::<Data>::uninit();
            ptr::write(data.as_mut_ptr().cast::<F>(), f);

            unsafe fn call<F: FnOnce(&Guard)>(raw: *mut u8, guard: &Guard) {
                let f: F = unsafe { ptr::read(raw.cast::<F>()) };
                f(guard);
            }

            Self {
                call: call::<F>,
                data,
                _marker: PhantomData,
            }
        }
    }

    /// Calls the function.
    #[inline]
    pub(crate) fn call(mut self, guard: &Guard) {
        let call = self.call;
        unsafe { call(self.data.as_mut_ptr().cast::<u8>(), guard) };
        forget(self);
    }
}

impl Drop for Task {
    fn drop(&mut self) {
        panic!("`Task` is dropped without being executed");
    }
}

#[cfg(test)]
mod tests {
    use super::Task;
    use crate::pin;
    use std::cell::Cell;

    #[test]
    fn single_word_data() {
        let fired = &Cell::new(false);

        let d = Task::new(move |_| {
            fired.set(true);
        });

        assert!(!fired.get());
        d.call(&pin());
        assert!(fired.get());
    }
}
