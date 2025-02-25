use std::{marker::PhantomData, ops::Deref, sync::atomic::Ordering};

// We use CIRC to quickly design the prototype API.
use circ::*;

/// Note: `SkipTrait` is not part of the API, but just for a temporary use.
/// We use `circ::{Rc, AtomicRc}` for the prototype design, but we are not interested in
/// CIRC's `RcObject` trait implementation, which enables the immediate recursive destruction.
/// So, we simply skip implementing it by using this struct.
struct SkipTrait<T>(T);

unsafe impl<T> RcObject for SkipTrait<T> {
    fn pop_edges(&mut self, _out: &mut Vec<Rc<Self>>) {}
}

impl<T> Deref for SkipTrait<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// A root-count-protected atomic reference to the managed object.
/// It can be sent and atomic with other threads. Before dereferencing,
/// you must create a `Local` reference to the same object by calling `load`.
///
/// It is interiorly-mutable, meaning that you can atomically update
/// the underlying reference with `&Atomic<T>`.
pub struct Atomic<T: Send> {
    ptr: AtomicRc<SkipTrait<T>>,
}

unsafe impl<T: Send + Sync> Sync for Atomic<T> {}
unsafe impl<T: Send + Sync> Send for Atomic<T> {}

impl<T: Send + Sync> Atomic<T> {
    pub fn new(item: T) -> Self {
        let ptr = AtomicRc::new(SkipTrait(item));
        Self { ptr }
    }

    pub fn null() -> Self {
        Self {
            ptr: AtomicRc::null(),
        }
    }

    pub fn load(&self) -> Option<Local<T>> {
        let guard = cs();
        let snapshot = self.ptr.load(Ordering::SeqCst, &guard);
        let ptr = snapshot.counted();
        if ptr.is_null() {
            None
        } else {
            Some(Local {
                ptr,
                _marker: PhantomData,
            })
        }
    }

    pub fn store(&self, local: Option<&Local<T>>) {
        let ptr = if let Some(local) = local {
            local.ptr.clone()
        } else {
            Rc::null()
        };
        self.ptr.store(ptr, Ordering::SeqCst, &cs());
    }

    pub fn take(&self) -> Option<Local<T>>
    where
        T: Default,
    {
        self.swap(None)
    }

    pub fn swap(&self, new: Option<&Local<T>>) -> Option<Local<T>> {
        let ptr = if let Some(new) = new {
            new.ptr.clone()
        } else {
            Rc::null()
        };
        let prev = self.ptr.swap(ptr, Ordering::SeqCst);
        if prev.is_null() {
            None
        } else {
            Some(Local {
                ptr: prev,
                _marker: PhantomData,
            })
        }
    }

    pub fn compare_exchange(&self, current: Option<&Local<T>>, new: Option<&Local<T>>) -> bool {
        let guard = &cs();
        let current = if let Some(current) = current {
            current.ptr.snapshot(guard)
        } else {
            Snapshot::null()
        };
        let new = if let Some(new) = new {
            new.ptr.clone()
        } else {
            Rc::null()
        };
        self.ptr
            .compare_exchange(current, new, Ordering::SeqCst, Ordering::SeqCst, guard)
            .is_ok()
    }
}

impl<T: Send + Sync> Clone for Atomic<T> {
    fn clone(&self) -> Self {
        if let Some(local) = self.load() {
            local.atomic()
        } else {
            Atomic::null()
        }
    }
}

/// A hazard-pointer-protected thread-local reference to the managed object.
/// To dereference, you must call `borrow` which creates an immuatable reference,
/// and (if the compaction is enabled) pins the allocation.
pub struct Local<T: Send + Sync> {
    ptr: Rc<SkipTrait<T>>,
    // A marker to prevent an implicit `Send + Sync` implementation.
    _marker: PhantomData<*const ()>,
}

impl<T: Send + Sync> Local<T> {
    pub fn new(item: T) -> Self {
        Self {
            ptr: Rc::new(SkipTrait(item)),
            _marker: PhantomData,
        }
    }

    pub fn borrow(&self) -> &T {
        // `unwrap` must succeed: `Local` does not allow a null pointer.
        self.ptr.as_ref().unwrap()
    }

    pub fn atomic(&self) -> Atomic<T> {
        Atomic {
            ptr: AtomicRc::from(&self.ptr),
        }
    }

    pub fn ptr_eq(&self, other: &Self) -> bool {
        self.ptr.ptr_eq(&other.ptr)
    }

    pub fn opt_ptr_eq(this: Option<&Self>, other: Option<&Self>) -> bool {
        match (this, other) {
            (None, None) => true,
            (None, Some(_)) | (Some(_), None) => false,
            (Some(x), Some(y)) => x.ptr_eq(y),
        }
    }
}

impl<T: Send + Sync> Clone for Local<T> {
    fn clone(&self) -> Self {
        Self {
            ptr: self.ptr.clone(),
            _marker: PhantomData,
        }
    }
}

impl<T: Send + Sync + PartialEq> PartialEq for Local<T> {
    fn eq(&self, other: &Self) -> bool {
        self.borrow() == other.borrow()
    }
}

impl<T: Send + Sync + Eq> Eq for Local<T> {}

/// A variant of `Atomic` that supports pointer tagging.
pub struct TaggedAtomic<T: Send + Sync> {
    ptr: AtomicRc<SkipTrait<T>>,
}

unsafe impl<T: Send + Sync> Sync for TaggedAtomic<T> {}
unsafe impl<T: Send + Sync> Send for TaggedAtomic<T> {}

impl<T: Send + Sync> TaggedAtomic<T> {
    pub fn new(item: T) -> Self {
        let ptr = AtomicRc::new(SkipTrait(item));
        Self { ptr }
    }

    pub fn null() -> Self {
        Self {
            ptr: AtomicRc::null(),
        }
    }

    pub fn load(&self) -> (Option<Local<T>>, usize) {
        let guard = cs();
        let snapshot = self.ptr.load(Ordering::SeqCst, &guard);
        let ptr = snapshot.counted();
        let tag = ptr.tag();
        let loaded = if ptr.is_null() {
            None
        } else {
            Some(Local {
                ptr,
                _marker: PhantomData,
            })
        };
        (loaded, tag)
    }

    pub fn store(&self, local: Option<&Local<T>>, tag: usize) {
        let ptr = if let Some(local) = local {
            local.ptr.clone()
        } else {
            Rc::null()
        };
        self.ptr.store(ptr.with_tag(tag), Ordering::SeqCst, &cs());
    }

    pub fn take(&self) -> (Option<Local<T>>, usize)
    where
        T: Default,
    {
        self.swap(None, 0)
    }

    pub fn swap(&self, new: Option<&Local<T>>, tag: usize) -> (Option<Local<T>>, usize) {
        let ptr = if let Some(new) = new {
            new.ptr.clone()
        } else {
            Rc::null()
        };
        let prev = self.ptr.swap(ptr.with_tag(tag), Ordering::SeqCst);
        let result = if prev.is_null() {
            None
        } else {
            Some(Local {
                ptr: prev,
                _marker: PhantomData,
            })
        };
        (result, tag)
    }

    pub fn compare_exchange(
        &self,
        current: (Option<&Local<T>>, usize),
        new: (Option<&Local<T>>, usize),
    ) -> bool {
        let guard = &cs();

        let current = if let Some(current) = current.0 {
            current.ptr.snapshot(guard)
        } else {
            Snapshot::null()
        }
        .with_tag(current.1);

        let new = if let Some(new) = new.0 {
            new.ptr.clone()
        } else {
            Rc::null()
        }
        .with_tag(new.1);

        self.ptr
            .compare_exchange(current, new, Ordering::SeqCst, Ordering::SeqCst, guard)
            .is_ok()
    }

    pub fn fetch_or_tag(&self, tag: usize) -> usize {
        // Note: In the real implementation, probably it will be wait-free.
        let guard = &cs();

        loop {
            let current = self.ptr.load(Ordering::SeqCst, guard);
            if self
                .ptr
                .compare_exchange_tag(current, tag, Ordering::SeqCst, Ordering::SeqCst, guard)
                .is_ok()
            {
                return current.tag();
            }
        }
    }
}

impl<T: Send + Sync> Clone for TaggedAtomic<T> {
    fn clone(&self) -> Self {
        let ptr = TaggedAtomic::null();
        if let (Some(local), tag) = self.load() {
            ptr.store(Some(&local), tag);
        }
        ptr
    }
}
