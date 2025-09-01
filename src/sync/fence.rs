//! Universal asymmetric barrier implementation.

extern crate libc;
extern crate windows_sys;

#[allow(unused_macros)]
macro_rules! fatal_assert {
    ($cond:expr) => {
        if !$cond {
            #[allow(unused_unsafe)]
            unsafe {
                libc::abort();
            }
        }
    };
}

cfg_if! {
    if #[cfg(all(target_os = "linux"))] {
        pub use linux::*;
    } else if #[cfg(target_os = "windows")] {
        pub use windows::*;
    } else if #[cfg(any(target_os = "macos", target_os = "ios"))] {
        pub use apple::*;
    } else {
        pub use default::*;
    }
}

#[allow(dead_code)]
mod default {
    use core::sync::atomic::{Ordering, fence};

    /// Issues a light memory barrier for fast path.
    ///
    /// It just issues the normal memory barrier instruction.
    #[inline]
    pub fn light() {
        fence(Ordering::SeqCst);
    }

    /// Issues a heavy memory barrier for slow path.
    ///
    /// It just issues the normal memory barrier instruction.
    #[inline]
    pub fn heavy() {
        fence(Ordering::SeqCst);
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use core::sync::atomic;
    use std::sync::LazyLock;

    /// A choice between three strategies for process-wide barrier on Linux.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Strategy {
        /// Use the `membarrier` system call.
        Membarrier,
        /// Use the `mprotect`-based trick.
        Mprotect,
        /// Use `SeqCst` fences.
        Fallback,
    }

    /// The right strategy to use on the current machine.
    static STRATEGY: LazyLock<Strategy> = LazyLock::new(|| {
        if membarrier::is_supported() {
            Strategy::Membarrier
        } else if mprotect::is_supported() {
            Strategy::Mprotect
        } else {
            Strategy::Fallback
        }
    });

    mod membarrier {
        /// Commands for the membarrier system call.
        ///
        /// # Caveat
        ///
        /// We're defining it here because, unfortunately, the `libc` crate currently doesn't
        /// expose `membarrier_cmd` for us. You can find the numbers in the [Linux source
        /// code](https://github.com/torvalds/linux/blob/master/include/uapi/linux/membarrier.h).
        ///
        /// This enum should really be `#[repr(libc::c_int)]`, but Rust currently doesn't allow it.
        #[repr(i32)]
        #[allow(dead_code, non_camel_case_types)]
        enum membarrier_cmd {
            MEMBARRIER_CMD_QUERY = 0,
            MEMBARRIER_CMD_GLOBAL = (1 << 0),
            MEMBARRIER_CMD_GLOBAL_EXPEDITED = (1 << 1),
            MEMBARRIER_CMD_REGISTER_GLOBAL_EXPEDITED = (1 << 2),
            MEMBARRIER_CMD_PRIVATE_EXPEDITED = (1 << 3),
            MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED = (1 << 4),
            MEMBARRIER_CMD_PRIVATE_EXPEDITED_SYNC_CORE = (1 << 5),
            MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED_SYNC_CORE = (1 << 6),
        }

        /// Call the `sys_membarrier` system call.
        #[inline]
        fn sys_membarrier(cmd: membarrier_cmd) -> libc::c_long {
            unsafe { libc::syscall(libc::SYS_membarrier, cmd as libc::c_int, 0 as libc::c_int) }
        }

        /// Returns `true` if the `sys_membarrier` call is available.
        pub fn is_supported() -> bool {
            // Queries which membarrier commands are supported. Checks if private expedited
            // membarrier is supported.
            let ret = sys_membarrier(membarrier_cmd::MEMBARRIER_CMD_QUERY);
            if ret < 0
                || ret & membarrier_cmd::MEMBARRIER_CMD_PRIVATE_EXPEDITED as libc::c_long == 0
                || ret & membarrier_cmd::MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED as libc::c_long
                    == 0
            {
                return false;
            }

            // Registers the current process as a user of private expedited membarrier.
            if sys_membarrier(membarrier_cmd::MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED) < 0 {
                return false;
            }

            true
        }

        /// Executes a heavy `sys_membarrier`-based barrier.
        #[inline]
        pub fn barrier() {
            fatal_assert!(sys_membarrier(membarrier_cmd::MEMBARRIER_CMD_PRIVATE_EXPEDITED) >= 0);
        }
    }

    mod mprotect {
        use core::{cell::UnsafeCell, mem::MaybeUninit, ptr, sync::atomic};
        use libc;
        use std::sync::LazyLock;

        struct Barrier {
            lock: UnsafeCell<libc::pthread_mutex_t>,
            page: u64,
            page_size: libc::size_t,
        }

        unsafe impl Sync for Barrier {}

        impl Barrier {
            /// Issues a process-wide barrier by changing access protections of a single mmap-ed
            /// page. This method is not as fast as the `sys_membarrier()` call, but works very
            /// similarly.
            #[inline]
            fn barrier(&self) {
                let page = self.page as *mut libc::c_void;

                unsafe {
                    // Lock the mutex.
                    fatal_assert!(libc::pthread_mutex_lock(self.lock.get()) == 0);

                    // Set the page access protections to read + write.
                    fatal_assert!(
                        libc::mprotect(page, self.page_size, libc::PROT_READ | libc::PROT_WRITE,)
                            == 0
                    );

                    // Ensure that the page is dirty before we change the protection so that we
                    // prevent the OS from skipping the global TLB flush.
                    let atomic_usize = &*(page as *const atomic::AtomicUsize);
                    atomic_usize.fetch_add(1, atomic::Ordering::SeqCst);

                    // Set the page access protections to none.
                    //
                    // Changing a page protection from read + write to none causes the OS to issue
                    // an interrupt to flush TLBs on all processors. This also results in flushing
                    // the processor buffers.
                    fatal_assert!(libc::mprotect(page, self.page_size, libc::PROT_NONE) == 0);

                    // Unlock the mutex.
                    fatal_assert!(libc::pthread_mutex_unlock(self.lock.get()) == 0);
                }
            }
        }

        /// An alternative solution to `sys_membarrier` that works on older Linux kernels and
        /// x86/x86-64 systems.
        static BARRIER: LazyLock<Barrier> = LazyLock::new(|| {
            unsafe {
                // Find out the page size on the current system.
                let page_size = libc::sysconf(libc::_SC_PAGESIZE);
                fatal_assert!(page_size > 0);
                let page_size = page_size as libc::size_t;

                // Create a dummy page.
                let page = libc::mmap(
                    ptr::null_mut(),
                    page_size,
                    libc::PROT_NONE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                    -1 as libc::c_int,
                    0 as libc::off_t,
                );
                fatal_assert!(page != libc::MAP_FAILED);
                fatal_assert!((page as libc::size_t).is_multiple_of(page_size));

                // Locking the page ensures that it stays in memory during the two mprotect
                // calls in `Barrier::barrier()`. If the page was unmapped between those calls,
                // they would not have the expected effect of generating IPI.
                libc::mlock(page, page_size as libc::size_t);

                // Initialize the mutex.
                let lock = UnsafeCell::new(libc::PTHREAD_MUTEX_INITIALIZER);
                let mut attr = MaybeUninit::<libc::pthread_mutexattr_t>::uninit();
                fatal_assert!(libc::pthread_mutexattr_init(attr.as_mut_ptr()) == 0);
                let mut attr = attr.assume_init();
                fatal_assert!(
                    libc::pthread_mutexattr_settype(&mut attr, libc::PTHREAD_MUTEX_NORMAL) == 0
                );
                fatal_assert!(libc::pthread_mutex_init(lock.get(), &attr) == 0);
                fatal_assert!(libc::pthread_mutexattr_destroy(&mut attr) == 0);

                let page = page as u64;

                Barrier {
                    lock,
                    page,
                    page_size,
                }
            }
        });

        /// Returns `true` if the `mprotect`-based trick is supported.
        pub fn is_supported() -> bool {
            cfg!(target_arch = "x86") || cfg!(target_arch = "x86_64")
        }

        /// Executes a heavy `mprotect`-based barrier.
        #[inline]
        pub fn barrier() {
            BARRIER.barrier();
        }
    }

    /// Issues a light memory barrier for fast path.
    ///
    /// It issues a compiler fence, which disallows compiler optimizations across itself. It incurs
    /// basically no costs in run-time.
    #[inline]
    #[allow(dead_code)]
    pub fn light() {
        use self::Strategy::*;
        match *STRATEGY {
            Membarrier | Mprotect => atomic::compiler_fence(atomic::Ordering::SeqCst),
            Fallback => atomic::fence(atomic::Ordering::SeqCst),
        }
    }

    /// Issues a heavy memory barrier for slow path.
    ///
    /// It issues a private expedited membarrier using the `sys_membarrier()` system call, if
    /// supported; otherwise, it falls back to `mprotect()`-based process-wide memory barrier.
    #[inline]
    #[allow(dead_code)]
    pub fn heavy() {
        use self::Strategy::*;
        match *STRATEGY {
            Membarrier => membarrier::barrier(),
            Mprotect => mprotect::barrier(),
            Fallback => atomic::fence(atomic::Ordering::SeqCst),
        }
    }
}

#[cfg(target_os = "windows")]
mod windows {
    use core::sync::atomic;
    use windows_sys;

    /// Issues light memory barrier for fast path.
    ///
    /// It issues compiler fence, which disallows compiler optimizations across itself.
    #[inline]
    pub fn light() {
        atomic::compiler_fence(atomic::Ordering::SeqCst);
    }

    /// Issues heavy memory barrier for slow path.
    ///
    /// It invokes the `FlushProcessWriteBuffers()` system call.
    #[inline]
    pub fn heavy() {
        unsafe {
            windows_sys::Win32::System::Threading::FlushProcessWriteBuffers();
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
mod apple {
    use core::sync::atomic;

    mod barrier {
        #![allow(non_camel_case_types)]
        #![allow(unused)]
        #![allow(non_snake_case)]

        use core::mem;
        use core::slice;

        use libc::{
            KERN_SUCCESS, mach_task_self, task_threads, thread_act_t, uintptr_t, vm_address_t,
            vm_deallocate,
        };

        // Include Raw FFI for `is_supported` and `flush_process_write_buffers`.
        include!(concat!(env!("OUT_DIR"), "/mach.rs"));

        /// Equivalent to `x86_THREAD_STATE64_COUNT` and `ARM_THREAD_STATE64_COUNT`
        /// macros in `<mach/thread_status.h>`
        const fn thread_state64_count() -> u32 {
            cfg_if! {
                if #[cfg(target_arch = "x86_64")] {
                    (mem::size_of::<x86_thread_state64_t>() / mem::size_of::<u32>()) as u32
                } else if #[cfg(target_arch = "aarch64")] {
                    (mem::size_of::<arm_thread_state64_t>() / mem::size_of::<u32>()) as u32
                } else {
                    // This path should not be reachable!
                    // Because we check if the heavy barrier is supported
                    // by `is_supported` function before using `flush_process_write_buffers`.
                    unreachable!()
                }
            }
        }

        /// Check if the heavy membarrier using an inter processor interrupt
        /// mechanism is supported on the host environment.
        ///
        /// An inter processor interrupt mechanism on Apple environments
        /// is implementable for only x64 and ARM64.
        #[inline]
        pub const fn is_supported() -> bool {
            cfg_if! {
                if #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))] {
                    true
                } else {
                    false
                }
            }
        }

        #[inline]
        fn assert_success(ret: kern_return_t, err_msg: &'static str) {
            if ret != KERN_SUCCESS as kern_return_t {
                panic!("{}", err_msg);
            }
        }

        /// Issue a heavy memory barrier.
        ///
        /// It flushes write buffers of executing threads of the current process,
        /// and is equivalent to `membarrier` on latest Linux and `FlushProcessWriteBuffers` on Windows.
        #[inline]
        pub unsafe fn flush_process_write_buffers() {
            let mut thread_count: mach_msg_type_number_t = mem::zeroed();
            let mut thread_acts: *mut thread_act_t = mem::zeroed();

            assert_success(
                task_threads(mach_task_self(), &mut thread_acts, &mut thread_count),
                "Failed to fetch thread information!",
            );

            let thread_acts_arr = slice::from_raw_parts_mut(thread_acts, thread_count as usize);
            let mut sp = mem::zeroed();
            let mut register_values: [uintptr_t; 128] = mem::zeroed();

            for act in thread_acts_arr {
                cfg_if! {
                    if #[cfg(register_pointer_values)] {
                        let mut registers = 128;
                        assert_success(
                            thread_get_register_pointer_values(*act, &mut sp, &mut registers, register_values.as_mut_ptr()),
                            "`thread_get_register_pointer_values` system call failed!"
                        );
                    } else if #[cfg(target_arch = "x86_64")] {
                        let mut thread_state: x86_thread_state64_t = mem::zeroed();
                        let mut count = thread_state64_count();
                        assert_success(
                            thread_get_state(*act, x86_THREAD_STATE64 as i32, (&mut thread_state) as *mut _ as _, &mut count),
                            "`thread_get_state` system call for x86 failed!"
                        );
                    } else if #[cfg(target_arch = "aarch64")] {
                        let mut thread_state: arm_thread_state64_t = mem::zeroed();
                        let mut count = thread_state64_count();
                        assert_success(
                            thread_get_state(*act, ARM_THREAD_STATE64 as i32, (&mut thread_state) as *mut _ as _, &mut count),
                            "`thread_get_state` system call for AARCH64 failed!"
                        );
                    } else {
                        // This path should not be reachable!
                        // Because we check if the heavy barrier is supported
                        // by `is_supported` function before using `flush_process_write_buffers`.
                        unreachable!()
                    }
                };

                assert_success(
                    mach_port_deallocate(mach_task_self(), *act),
                    "Failed to decrement the port right's reference count!",
                );
            }

            assert_success(
                vm_deallocate(
                    mach_task_self(),
                    thread_acts as vm_address_t,
                    thread_count as usize * mem::size_of::<thread_act_t>(),
                ),
                "Failed to deallocate the used thread list!",
            );
        }
    }

    /// Issues a light memory barrier for fast path.
    ///
    /// It issues a compiler fence, which disallows compiler optimizations across itself. It incurs
    /// basically no costs in run-time.
    #[inline]
    pub fn light() {
        if barrier::is_supported() {
            atomic::compiler_fence(atomic::Ordering::SeqCst);
        } else {
            atomic::fence(atomic::Ordering::SeqCst);
        }
    }

    /// Issues heavy memory barrier for slow path.
    ///
    /// It flushes write buffers of executing threads of the current process
    /// by Inter Process Interrupt(IPI) mechanism.
    ///
    /// In the latest version of MacOS(at least 10.14) and iOS(at least 12),
    /// it requests the threads pointer values to force the thread to emit a
    /// memory barrier. In older versions, it falls back to the `thread_get_state`
    /// -based method.
    #[inline]
    pub fn heavy() {
        if barrier::is_supported() {
            unsafe { barrier::flush_process_write_buffers() };
        } else {
            atomic::fence(atomic::Ordering::SeqCst);
        }
    }
}
