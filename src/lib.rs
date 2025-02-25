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

/// A root-count-protected atomic reference to the shared managed object.
/// It can be sent and shared with other threads. Before dereferencing,
/// you must create a `Local` reference to the same object by calling `load`.
///
/// It is interiorly-mutable, meaning that you can atomically update
/// the underlying reference with `&Shared<T>`.
pub struct Shared<T: Send> {
    ptr: AtomicRc<SkipTrait<T>>,
}

unsafe impl<T: Send + Sync> Sync for Shared<T> {}
unsafe impl<T: Send + Sync> Send for Shared<T> {}

impl<T: Send> Shared<T> {
    pub fn new(item: T) -> Self {
        let ptr = AtomicRc::new(SkipTrait(item));
        Self { ptr }
    }

    pub fn null() -> Self {
        Self {
            ptr: AtomicRc::null(),
        }
    }

    pub fn load(&self) -> Local<T> {
        let guard = cs();
        let snapshot = self.ptr.load(Ordering::SeqCst, &guard);
        let ptr = snapshot.counted();
        Local {
            ptr,
            _marker: PhantomData,
        }
    }

    pub fn store(&self, local: &Local<T>) {
        self.ptr.store(local.ptr.clone(), Ordering::SeqCst, &cs());
    }

    pub fn take(&self) -> Local<T>
    where
        T: Default,
    {
        Local {
            ptr: self
                .ptr
                .swap(Rc::new(SkipTrait(T::default())), Ordering::SeqCst),
            _marker: PhantomData,
        }
    }

    pub fn swap(&self, new: &Local<T>) -> Local<T> {
        Local {
            ptr: self.ptr.swap(new.ptr.clone(), Ordering::SeqCst),
            _marker: PhantomData,
        }
    }

    pub fn compare_exchange(&self, current: &Local<T>, new: &Local<T>) -> bool {
        let guard = &cs();
        self.ptr
            .compare_exchange(
                current.ptr.snapshot(guard),
                new.ptr.clone(),
                Ordering::SeqCst,
                Ordering::SeqCst,
                guard,
            )
            .is_ok()
    }
}

impl<T: Send> Clone for Shared<T> {
    fn clone(&self) -> Self {
        self.load().shared()
    }
}

/// A hazard-pointer-protected thread-local reference to the shared managed object.
/// To dereference, you must call `borrow` which creates an immuatable reference,
/// and (if the compaction is enabled) pins the allocation.
pub struct Local<T: Send> {
    ptr: Rc<SkipTrait<T>>,
    // A marker to prevent an implicit `Send + Sync` implementation.
    _marker: PhantomData<*const ()>,
}

impl<T: Send> Local<T> {
    pub fn new(item: T) -> Self {
        Self {
            ptr: Rc::new(SkipTrait(item)),
            _marker: PhantomData,
        }
    }

    pub fn null() -> Self {
        Self {
            ptr: Rc::null(),
            _marker: PhantomData,
        }
    }

    pub fn is_null(&self) -> bool {
        self.ptr.is_null()
    }

    pub fn as_ref(&self) -> Option<&T> {
        self.ptr.as_ref().map(|inner| &inner.0)
    }

    pub fn shared(&self) -> Shared<T> {
        Shared {
            ptr: AtomicRc::from(&self.ptr),
        }
    }

    pub fn tag(&self) -> usize {
        self.ptr.tag()
    }

    pub fn with_tag(self, tag: usize) -> Self {
        Self {
            ptr: self.ptr.with_tag(tag),
            _marker: PhantomData,
        }
    }
}

impl<T: Send> Clone for Local<T> {
    fn clone(&self) -> Self {
        Self {
            ptr: self.ptr.clone(),
            _marker: PhantomData,
        }
    }
}
