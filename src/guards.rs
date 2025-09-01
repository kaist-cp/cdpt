//! RAII-style guards for users.

use crate::{
    TraceObj,
    epoch::{Color, Epoch, Phase},
    internal::{Local, OBJ_BATCH_SIZE},
    pointers::ManObj,
    sync::{Entry, fence},
    task::Task,
    tls::global,
};
use crossbeam::epoch::pin as ebr_pin;
use std::{
    cell::{Cell, LazyCell},
    rc::Rc,
    sync::atomic::Ordering,
};

const HELP_NORMAL_MAX_TRIAL: usize = 2;
const HELP_TRACING_MAX_TRIAL: usize = 4;

/// A handle to a garbage collector.
#[derive(Clone)]
pub struct Handle {
    pub(crate) local: Rc<Entry<Local>>,
}

impl Handle {
    #[inline]
    pub(crate) fn local(&self) -> &Local {
        &self.local
    }

    /// Pins the handle.
    #[inline]
    pub fn pin(&self) -> Guard {
        let guard = Guard {
            local: self.local.clone(),
        };
        self.local().pin_inner();
        guard
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

pub struct Guard {
    pub(crate) local: Rc<Entry<Local>>,
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
        &self.local
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
        unsafe { self.local().pinned_alloc_color() }
    }

    pub(crate) fn global_phase(&self) -> Phase {
        fence::light();
        global().load_epoch().phase()
    }

    pub(crate) fn alloc<T: 'static + TraceObj>(&self, obj: ManObj<T>) -> *mut ManObj<T> {
        self.local().alloc(obj, self)
    }

    pub(crate) fn schedule_mark<T: 'static + TraceObj>(&self, obj: &ManObj<T>) {
        self.local().schedule_mark(obj, self);
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
    pub(crate) fn help_normal(&self) {
        if self.phase() != Phase::N {
            return;
        }

        call_without_recursion(&self.local().is_helping_normal, || self.help_normal_inner());
    }

    #[inline]
    fn help_normal_inner(&self) {
        let ebr_guard = &ebr_pin();
        let mut trial_count = 0;

        for q_idx in self.local().generate_shard_permut() {
            let prev_white = self.black_color();
            let marked_q = &global().marked_objs[prev_white as usize][q_idx];

            while let Some(batch) = marked_q.try_pop(ebr_guard) {
                for obj in batch.0 {
                    if prev_white == obj.color() {
                        drop(obj);
                        continue;
                    }
                    unsafe { self.local().push_fresh_obj(obj) };
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
    pub(crate) fn help_tracing(&self) {
        if self.phase() != Phase::RT && self.phase() != Phase::CT {
            return;
        }

        if !self.is_tracing_synced() {
            return;
        }

        call_without_recursion(&self.local().is_helping_root_tracing, || {
            self.help_root_tracing()
        });
        call_without_recursion(&self.local().is_helping_draining_mark_tasks, || {
            self.help_draining_mark_tasks_inner()
        });
    }

    #[inline]
    fn is_tracing_synced(&self) -> bool {
        // Ensure that all previous Normal phases have ended.
        // Note: If some threads (T_m) are helping with marking tasks
        // while others (T_s) are helping with sweeping tasks,
        // T_s may misinterpret a black object (i.e., marked by T_m)
        // as an unreachable object (i.e., still having the previous allocation color),
        // which can lead to a use-after-free error.
        global().locals.iter_using().all(|local| {
            let other_epoch = local.epoch.load(Ordering::Relaxed);
            !other_epoch.is_pinned() || other_epoch.phase() != Phase::N
        })
    }

    #[inline]
    pub(crate) fn help_draining_mark_tasks(&self) {
        if self.phase() != Phase::RT && self.phase() != Phase::CT {
            return;
        }

        if !self.is_tracing_synced() {
            return;
        }

        call_without_recursion(&self.local().is_helping_draining_mark_tasks, || {
            self.help_draining_mark_tasks_inner()
        });
    }

    #[inline]
    pub fn help_draining_mark_tasks_inner(&self) {
        let mt_len = unsafe { &*self.local().mark_tasks.get() }.len();
        for _ in 0..(mt_len.min(OBJ_BATCH_SIZE / 2)) {
            if let Some(task) = self.try_pop_mark_task() {
                task.call();
                continue;
            }
            break;
        }
    }

    #[inline]
    fn help_root_tracing(&self) {
        if self.phase() != Phase::RT {
            return;
        }

        let ebr_guard = &ebr_pin();
        let hazards = LazyCell::new(|| self.local().scan_or_reuse_hazards(self, ebr_guard));
        let mut trial_count = 0;

        for q_idx in self.local().generate_shard_permut() {
            let fresh_q = &global().fresh_objs[self.white_color() as usize][q_idx];
            while let Some(batch) = fresh_q.try_pop(ebr_guard) {
                for obj in &batch.0 {
                    if obj.root_count() > 0 || hazards.contains(&obj.address()) {
                        obj.mark(self);
                    }
                }
                let marked_q_idx = self.local().select_obj_shard();
                let marked_q = &global().marked_objs[self.white_color() as usize][marked_q_idx];
                marked_q.push(batch, ebr_guard);

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

#[inline]
fn call_without_recursion(flag: &Cell<bool>, f: impl FnOnce()) {
    if flag.get() {
        return;
    }
    flag.set(true);
    f();
    flag.set(false);
}
