//! RAII-style guards for users.

use crate::{
    TraceObj,
    epoch::{Color, Epoch, Phase},
    internal::{Local, OBJ_BATCH_SIZE},
    pointers::ManObj,
    sync::fence,
    task::Task,
    tls::global,
};
use crossbeam::epoch::pin as ebr_pin;
use std::{cell::LazyCell, ptr::NonNull, sync::atomic::Ordering};

const HELP_NORMAL_MAX_TRIAL: usize = 2;
const HELP_TRACING_MAX_TRIAL: usize = 2;

/// A handle to a garbage collector.
pub struct Handle {
    pub(crate) local: NonNull<Local>,
}

impl Handle {
    #[inline]
    pub(crate) fn local(&self) -> &Local {
        unsafe { self.local.as_ref() }
    }

    /// Pins the handle.
    #[inline]
    pub fn pin(&self) -> Guard {
        self.local().pin()
    }

    /// Returns `true` if the handle is pinned.
    #[inline]
    pub fn is_pinned(&self) -> bool {
        self.local().is_pinned()
    }

    /// Helps the ongoing collection works if possible.
    #[inline]
    pub fn help_collect(&self) {
        self.pin().help_collect();
    }
}

impl Clone for Handle {
    fn clone(&self) -> Self {
        unsafe { Local::acquire_handle(self.local.as_ref()) };
        Self { local: self.local }
    }
}

impl Drop for Handle {
    #[inline]
    fn drop(&mut self) {
        unsafe { Local::release_handle(self.local.as_ref()) };
    }
}

pub struct Guard {
    pub(crate) local: NonNull<Local>,
}

impl Guard {
    /// Unpins and then immediately re-pins the thread.
    ///
    /// This method is useful when you don't want delay the advancement of the global epoch by
    /// holding an old epoch. For safety, you should not maintain any guard-based reference across
    /// the call (the latter is enforced by `&mut self`). The thread will only be repinned if this
    /// is the only active guard for the current thread.
    pub fn repin(&mut self) {
        self.local().repin();
    }

    pub(crate) fn local(&self) -> &Local {
        unsafe { self.local.as_ref() }
    }

    pub(crate) fn local_epoch(&self) -> Epoch {
        unsafe { self.local().pinned_epoch() }
    }

    pub(crate) fn phase(&self) -> Phase {
        self.local_epoch().phase()
    }

    pub(crate) fn white_color(&self) -> Color {
        self.local_epoch().color()
    }

    pub(crate) fn black_color(&self) -> Color {
        self.white_color().flip()
    }

    pub(crate) fn alloc_color(&self) -> Color {
        // TODO: We may want to relax this (e.g., white for `Local` even during tracing).
        match self.phase() {
            Phase::N => self.white_color(),
            _ => self.black_color(),
        }
    }

    pub(crate) fn global_phase(&self) -> Phase {
        fence::light();
        global().load_epoch().phase()
    }

    pub(crate) fn alloc<T: 'static + TraceObj>(&self, obj: ManObj<T>) -> *mut ManObj<T> {
        self.local().alloc(obj, self)
    }

    pub(crate) fn schedule_mark<T: 'static + TraceObj>(&self, obj: &ManObj<T>) {
        unsafe { self.local().schedule_mark(obj) };
    }

    pub(crate) fn try_pop_mark_task(&self) -> Option<Task> {
        self.local().try_pop_mark_task()
    }

    /// Helps the ongoing collection works if possible.
    pub fn help_collect(&self) {
        match self.phase() {
            Phase::N => self.help_normal(),
            Phase::RT | Phase::CT => self.help_tracing(),
        }
    }

    /// Helps sweeping works for the current Normal phase.
    #[inline]
    fn help_normal(&self) {
        let ebr_guard = &ebr_pin();
        let mut trial_count = 0;

        for q_idx in self.local().generate_shard_permut() {
            let prev_white = self.local_epoch().color().flip();
            let marked_q = &global().marked_objs[prev_white as usize][q_idx];

            loop {
                if let Some(batch) = marked_q.try_pop(ebr_guard) {
                    for obj in batch.0 {
                        if prev_white == obj.color() {
                            drop(obj);
                            continue;
                        }
                        unsafe { self.local().push_mark_obj(obj) };
                    }
                }

                trial_count += 1;
                if trial_count >= HELP_NORMAL_MAX_TRIAL {
                    return;
                }
            }
        }
    }

    /// Helps root marking and tracing works for the current RT or CT phase.
    #[inline]
    fn help_tracing(&self) {
        let mt_len = unsafe { &*self.local().mark_tasks.get() }.len();
        for _ in 0..(mt_len.min(OBJ_BATCH_SIZE / 2)) {
            if let Some(task) = self.try_pop_mark_task() {
                task.call();
                continue;
            }
            break;
        }

        if self.local_epoch().phase() != Phase::RT {
            return;
        }

        let ebr_guard = &ebr_pin();
        let synced = global().iter_locals(ebr_guard).all(|local| {
            self.local_epoch().timestamp() <= local.epoch.load(Ordering::Relaxed).timestamp()
        });
        if !synced {
            return;
        }

        let hazards = LazyCell::new(|| global().collect_hps(ebr_guard));
        let mut trial_count = 0;

        for q_idx in self.local().generate_shard_permut() {
            let fresh_q = &global().fresh_objs[self.white_color() as usize][q_idx];
            loop {
                if let Some(batch) = fresh_q.try_pop(ebr_guard) {
                    for obj in &batch.0 {
                        if obj.root_count() > 0 || hazards.contains(&obj.address()) {
                            obj.mark(self);
                        }
                    }
                    let marked_q_idx = self.local().select_obj_shard();
                    let marked_q = &global().marked_objs[self.white_color() as usize][marked_q_idx];
                    marked_q.push(batch, ebr_guard);
                }

                trial_count += 1;
                if trial_count >= HELP_TRACING_MAX_TRIAL {
                    return;
                }
            }
        }
    }
}

impl Drop for Guard {
    #[inline]
    fn drop(&mut self) {
        self.local().unpin_inner();
    }
}
